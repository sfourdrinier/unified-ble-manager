//! JNI facade of the process-owned mobile owner (`ubm-mobile`), class
//! `com.ubm.core.MobileCoreBridge` (see its Java doc for the frozen shape).
//!
//! No path from here reaches the staged/fake radio: this module uses only
//! `ubm_mobile` (guarded by `mobile_surface_has_no_staged_path`).
//!
//! Rust → Java: every [`RadioRequest`] becomes one typed `RadioHost` call
//! ([`request_call`] is the single mapping, unit-tested without a JVM).
//! A Java exception thrown by the host answers the request as a platform
//! failure instead of leaving it pending. Java → Rust: typed
//! `nativeComplete*` / `nativeIngest*` entries rebuild the typed values.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use jni::errors::{Error as JniError, ErrorPolicy};
use jni::objects::{
    JBooleanArray, JByteArray, JClass, JIntArray, JLongArray, JObject, JObjectArray, JString,
    Reference as _,
};
use jni::refs::Global;
use jni::signature::{MethodSignature, RuntimeMethodSignature};
use jni::strings::JNIString;
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring};
use jni::{Env, EnvUnowned, JValue, JavaVM};
#[cfg(ubm_jni_roundtrip)]
use ubm_core::oversized_evidence::{
    EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureLimits,
    ScanFailureMember, ScanFailureOccurrence,
};
#[cfg(ubm_jni_roundtrip)]
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota};
#[cfg(ubm_jni_roundtrip)]
use ubm_desktop::continuation_outbox::Outbox as TestOutbox;
#[cfg(ubm_jni_roundtrip)]
use ubm_desktop::continuation_outbox::WakeSink as TestWakeSink;
use ubm_desktop::continuation_outbox::{
    PendingScanFailure, ScanFailureCleanupRetirement, ScanFailureHead, ScanFailureRawToken,
};
use ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease;
use ubm_desktop::{
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopError, ObservedDelivery,
    PropertyFlags, ReadProvenance, ServiceSnapshot,
};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    AuthenticationState, BondState, BondedPeer, CloseFailure, CompanionRecord, CompletionStatus,
    ConnectedPeer, EncryptionState, FailureKind, HostOptions, IngressClass, IngressStatus,
    Instance, ManufacturerData, MobileHost, MobilePlatform, PhyObservation, PlatformFailure,
    PlatformRadio, RadioCompletion, RadioIngress, RadioRequest, ResolvedPeer, RestoredPeer,
    SecureConnectionsState, SecurityState, ServiceData, WakeSink,
};

use crate::build_identity::ubm_build_identity_json;

mod recording_stop_gate;

const EXCEPTION_CLASS: &str = "com/ubm/core/MobileCoreBridge$MobileCoreException";
/// `MobileCoreBridge.ABSENT_INT`.
const ABSENT_INT: jint = jint::MIN;
/// `MobileCoreBridge.STATUS_NO_HOST`.
const STATUS_NO_HOST: jint = -1;

// -- process host slot -------------------------------------------------

#[derive(Default)]
struct HostSlot {
    host: Option<Arc<MobileHost>>,
    installing: bool,
    next_binding: i64,
    bindings: HashMap<i64, Arc<MobileHost>>,
    binding_radios: HashMap<i64, Arc<Global<JObject<'static>>>>,
    binding_owners: HashMap<i64, ubm_mobile::OrderedAdmissionOwner>,
    unpublished: HashMap<i64, UnpublishedBinding>,
    shutting_down: HashSet<i64>,
    retired: VecDeque<(i64, String)>,
    // Foreign recovery capabilities are never serialized.  This registry is
    // deliberately per JNI binding: a handle from another React context, a
    // former binding, or a torn-down host is not an authority.
    scan_recovery: HashMap<i64, ScanRecoveryHandles>,
    scan_recovery_epochs: HashMap<i64, u64>,
    terminal_evidence_recovery: HashMap<i64, TerminalEvidenceRecoveryHandles>,
    terminal_evidence_recovery_epochs: HashMap<i64, u64>,
    #[cfg(ubm_jni_roundtrip)]
    test_radios: HashMap<i64, Weak<JniRadio>>,
}

#[derive(Default)]
struct TerminalEvidenceRecoveryHandles {
    epoch: u64,
    next: u64,
    lease_id: Option<String>,
    lease: Option<Arc<ubm_mobile::TerminalEvidenceRecovery>>,
    entries: HashMap<String, ubm_mobile::TerminalEvidenceRecoveryEntry>,
    // Acknowledgement replies can be lost after native has retired the row.
    // Keep a bounded, lease-local duplicate-cleanup window for those exact
    // opaque handles. A new recovery lease clears it; an evicted handle takes
    // the normal explicit lifecycle-invalid-state path.
    acknowledged: HashSet<String>,
    acknowledged_order: VecDeque<String>,
}

const TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY: usize = 256;

impl TerminalEvidenceRecoveryHandles {
    fn entry_alias(
        &mut self,
        binding: jlong,
        entry: &ubm_mobile::TerminalEvidenceRecoveryEntry,
    ) -> MobileResult<String> {
        if let Some((id, _)) = self.entries.iter().find(|(_, current)| *current == entry) {
            return Ok(id.clone());
        }
        let id = terminal_evidence_recovery_id(self, binding, "entry")?;
        self.entries.insert(id.clone(), entry.clone());
        Ok(id)
    }

    fn retain_acknowledgement(&mut self, handle: String) {
        if !self.acknowledged.insert(handle.clone()) {
            return;
        }
        self.acknowledged_order.push_back(handle);
        if self.acknowledged_order.len() > TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY {
            if let Some(expired) = self.acknowledged_order.pop_front() {
                self.acknowledged.remove(&expired);
            }
        }
    }

    fn clear_lease(&mut self) {
        self.lease_id = None;
        self.lease = None;
        self.entries.clear();
        self.acknowledged.clear();
        self.acknowledged_order.clear();
    }
}

struct ScanRecoveryHandles {
    epoch: u64,
    next: u64,
    limit: usize,
    leases: HashMap<String, Arc<ScanFailureRecoveryLease>>,
    leases_by_recording: HashMap<String, String>,
    owners: HashMap<String, (String, PendingScanFailure)>,
    heads: HashMap<String, (String, ScanFailureHead)>,
    cleanup: HashMap<String, (String, ScanFailureCleanupRetirement)>,
}

const MAX_SCAN_RECOVERY_LIVE_HANDLES: usize = 1024;
#[cfg(ubm_jni_roundtrip)]
static TEST_RECOVERY_OCCURRENCE: AtomicU64 = AtomicU64::new(1);

impl Default for ScanRecoveryHandles {
    fn default() -> Self {
        Self {
            next: 0,
            epoch: 0,
            limit: MAX_SCAN_RECOVERY_LIVE_HANDLES,
            leases: HashMap::new(),
            leases_by_recording: HashMap::new(),
            owners: HashMap::new(),
            heads: HashMap::new(),
            cleanup: HashMap::new(),
        }
    }
}

impl ScanRecoveryHandles {
    fn next_id(&mut self, binding: i64, kind: &str) -> MobileResult<String> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("scan.failure-recovery.handle", "handle namespace exhausted"))?;
        Ok(format!(
            "scan-recovery-{kind}-{binding}-{}-{}",
            self.epoch, self.next
        ))
    }

    fn lease(&self, id: &str) -> MobileResult<Arc<ScanFailureRecoveryLease>> {
        self.leases.get(id).cloned().ok_or_else(|| {
            invalid(
                "scan.failure-recovery.handle",
                "unknown or revoked recovery lease",
            )
        })
    }

    fn owner(&self, id: &str) -> MobileResult<(Arc<ScanFailureRecoveryLease>, PendingScanFailure)> {
        let (lease, owner) = self.owners.get(id).cloned().ok_or_else(|| {
            invalid(
                "scan.failure-recovery.owner",
                "unknown or revoked recovery owner",
            )
        })?;
        Ok((self.lease(&lease)?, owner))
    }

    fn head(&self, id: &str) -> MobileResult<(Arc<ScanFailureRecoveryLease>, ScanFailureHead)> {
        let (lease, head) = self.heads.get(id).cloned().ok_or_else(|| {
            invalid(
                "scan.failure-recovery.head",
                "unknown or revoked recovery head",
            )
        })?;
        Ok((self.lease(&lease)?, head))
    }

    fn cleanup(
        &self,
        id: &str,
    ) -> MobileResult<(Arc<ScanFailureRecoveryLease>, ScanFailureCleanupRetirement)> {
        let (lease, receipt) = self.cleanup.get(id).cloned().ok_or_else(|| {
            invalid(
                "scan.failure-recovery.cleanup",
                "unknown or revoked cleanup receipt",
            )
        })?;
        Ok((self.lease(&lease)?, receipt))
    }

    fn live_handle_count(&self) -> usize {
        self.leases.len() + self.owners.len() + self.heads.len() + self.cleanup.len()
    }

    fn ensure_room(&self, operation: &'static str) -> MobileResult<()> {
        self.ensure_additional_room(1, operation)
    }

    fn ensure_additional_room(&self, count: usize, operation: &'static str) -> MobileResult<()> {
        if self.live_handle_count().saturating_add(count) > self.limit {
            return Err(invalid(
                operation,
                "scan recovery live-handle limit reached",
            ));
        }
        Ok(())
    }

    fn reserve_head_replacement(&self, operation: &'static str) -> MobileResult<()> {
        // Finalization removes its currently-live head before publishing one
        // cleanup receipt, so a full table can atomically replace that slot.
        if self.live_handle_count() > self.limit {
            return Err(invalid(
                operation,
                "scan recovery live-handle limit reached",
            ));
        }
        Ok(())
    }

    fn issue_owner_batch(
        &mut self,
        binding: i64,
        lease_id: &str,
        values: &[PendingScanFailure],
    ) -> MobileResult<Vec<String>> {
        let mut missing = Vec::new();
        for value in values {
            if self.owner_alias(lease_id, value).is_none()
                && !missing
                    .iter()
                    .any(|existing: &PendingScanFailure| existing == value)
            {
                missing.push(*value);
            }
        }
        self.ensure_additional_room(missing.len(), "scan.failure-recovery.owners")?;
        let mut created = Vec::with_capacity(missing.len());
        for value in missing {
            created.push((value, self.next_id(binding, "owner")?));
        }
        for (value, id) in created {
            self.owners.insert(id, (lease_id.to_owned(), value));
        }
        values
            .iter()
            .map(|value| {
                self.owner_alias(lease_id, value).ok_or_else(|| {
                    invalid(
                        "scan.failure-recovery.owners",
                        "owner alias issuance failed",
                    )
                })
            })
            .collect()
    }

    fn issue_cleanup_batch(
        &mut self,
        binding: i64,
        lease_id: &str,
        values: &[ScanFailureCleanupRetirement],
    ) -> MobileResult<Vec<String>> {
        let mut missing = Vec::new();
        for value in values {
            if self.cleanup_alias(lease_id, value).is_none()
                && !missing
                    .iter()
                    .any(|existing: &ScanFailureCleanupRetirement| existing == value)
            {
                missing.push(value.clone());
            }
        }
        self.ensure_additional_room(missing.len(), "scan.failure-recovery.cleanup-receipts")?;
        let mut created = Vec::with_capacity(missing.len());
        for value in missing {
            created.push((value, self.next_id(binding, "cleanup")?));
        }
        for (value, id) in created {
            self.cleanup.insert(id, (lease_id.to_owned(), value));
        }
        values
            .iter()
            .map(|value| {
                self.cleanup_alias(lease_id, value).ok_or_else(|| {
                    invalid(
                        "scan.failure-recovery.cleanup-receipts",
                        "cleanup alias issuance failed",
                    )
                })
            })
            .collect()
    }

    fn lease_for_recording(&self, recording_id: &str) -> Option<String> {
        self.leases_by_recording.get(recording_id).cloned()
    }

    fn owner_alias(&self, lease_id: &str, owner: &PendingScanFailure) -> Option<String> {
        self.owners.iter().find_map(|(id, (lease, existing))| {
            (lease == lease_id && existing == owner).then(|| id.clone())
        })
    }

    fn head_alias(&self, lease_id: &str, head: &ScanFailureHead) -> Option<String> {
        self.heads.iter().find_map(|(id, (lease, existing))| {
            (lease == lease_id && existing == head).then(|| id.clone())
        })
    }

    fn cleanup_alias(
        &self,
        lease_id: &str,
        receipt: &ScanFailureCleanupRetirement,
    ) -> Option<String> {
        self.cleanup.iter().find_map(|(id, (lease, existing))| {
            (lease == lease_id && existing == receipt).then(|| id.clone())
        })
    }

    /// Remove a fully retired lease and every alias which only existed to
    /// access it. We never evict a live authority: the registry must report no
    /// remaining owners, and no head or cleanup receipt may still rely on it.
    /// This permits a long-lived process to commission more than the bounded
    /// number of recordings over time without invalidating an in-flight one.
    fn reclaim_terminal_lease(&mut self, lease_id: &str) {
        let Some(lease) = self.leases.get(lease_id) else {
            return;
        };
        if !lease.owners().is_empty()
            || self.heads.values().any(|(id, _)| id == lease_id)
            || self.cleanup.values().any(|(id, _)| id == lease_id)
        {
            return;
        }
        self.owners.retain(|_, (id, _)| id != lease_id);
        self.leases.remove(lease_id);
        self.leases_by_recording.retain(|_, id| id != lease_id);
    }
}

// Live debt is never evicted. Terminal receipts retain the most recent 256
// exact results; an older evicted token becomes unknown, never a new owner.
const MAX_RETAINED_BINDINGS: usize = 32;
const MAX_RETIRED_RECEIPTS: usize = 256;

fn remember_retirement(slot: &mut HostSlot, binding: i64, record: String) {
    if slot.retired.len() == MAX_RETIRED_RECEIPTS {
        slot.retired.pop_front();
    }
    slot.retired.push_back((binding, record));
}

// This record exists before Java receives the token. Once host construction is
// entered, only the mobile owner can prove its own cleanup; JNI must not infer
// that proof from an error returned by open.
struct UnpublishedBinding {
    owner: Option<ubm_mobile::OrderedAdmissionOwner>,
    retirement: ubm_mobile::OrderedAdmissionOwner,
    safe_to_retire: bool,
    fence_confirmed: bool,
    next_request: i64,
    pending: Option<(i64, mpsc::SyncSender<RadioCompletion>)>,
}

fn retirement_identity(binding: i64) -> MobileResult<ubm_mobile::OrderedAdmissionOwner> {
    let n = binding as u64;
    let value = format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (n >> 31) as u32,
        (n >> 15) & 0xffff,
        (n >> 3) & 0xfff,
        (n & 7) << 8,
        0
    );
    ubm_mobile::OrderedAdmissionOwner::negotiate_slot(2, &value)
        .map_err(|detail| invalid("mobile.host.install", detail))
}

fn host_slot() -> &'static Mutex<HostSlot> {
    static SLOT: OnceLock<Mutex<HostSlot>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HostSlot::default()))
}

fn recording_stop_permitted(slot: &HostSlot, owner: Option<i64>) -> bool {
    let bindings: HashSet<i64> = slot
        .binding_radios
        .keys()
        .chain(slot.bindings.keys())
        .copied()
        .collect();
    recording_stop_gate::permits_stop(
        owner,
        slot.host.is_some(),
        slot.installing,
        &bindings.into_iter().collect::<Vec<_>>(),
    )
}

fn recording_stop_owner_conflict() -> String {
    ubm_desktop::continuation::envelope(Err(serde_json::json!({
        "code": "storage.owner-conflict", "domain": "restoration",
        "operation": "continuation.recording.stop",
        "detail": "recording owner cannot be established while another native host is retained"
    })))
}

// Reserve the singleton without holding its lock across calls into Java.
// Dropping the reservation on any error (or unwind) permits a later retry.
struct HostInstallation {
    binding: Option<i64>,
}

impl HostInstallation {
    fn reserve(tagged: bool) -> Result<Self, &'static str> {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.installing || slot.host.is_some() || (!tagged && !slot.binding_radios.is_empty()) {
            return Err("host installation unavailable: legacy owner or concurrent installation");
        }
        if tagged && slot.binding_radios.len() >= MAX_RETAINED_BINDINGS {
            return Err("retained binding limit reached");
        }
        // Tokens are never reused, even when installation fails.
        let binding = if tagged {
            let next = slot
                .next_binding
                .checked_add(1)
                .ok_or("binding identities exhausted")?;
            slot.next_binding = next;
            Some(next)
        } else {
            None
        };
        slot.installing = true;
        Ok(Self { binding })
    }

    fn publish(self, host: Arc<MobileHost>) {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(binding) = self.binding {
            slot.unpublished.remove(&binding);
            slot.bindings.insert(binding, host);
        } else {
            slot.host = Some(host);
        }
        slot.installing = false;
        drop(slot);
        std::mem::forget(self);
    }
}

impl Drop for HostInstallation {
    fn drop(&mut self) {
        host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .installing = false;
    }
}

fn binding_host(binding: jlong) -> Option<Arc<MobileHost>> {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .bindings
        .get(&binding)
        .cloned()
}

fn legacy_host() -> Option<MobileHost> {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .host
        .as_ref()
        .map(|host| host.as_ref().clone())
}

// The legacy synthetic ordered harness opens only a legacy host.
#[cfg(ubm_jni_roundtrip)]
fn current_host() -> Option<MobileHost> {
    legacy_host()
}

// -- errors ------------------------------------------------------------

#[derive(Debug)]
enum MobileError {
    Contract(DesktopError),
    Jni(JniError),
}

impl From<JniError> for MobileError {
    fn from(error: JniError) -> Self {
        Self::Jni(error)
    }
}

impl From<DesktopError> for MobileError {
    fn from(error: DesktopError) -> Self {
        Self::Contract(error)
    }
}

type MobileResult<T> = Result<T, MobileError>;

fn wire_message(error: &DesktopError) -> String {
    format!(
        "{}|{}|{}|{}",
        error.code_str(),
        error.domain().as_str(),
        error.operation(),
        error.detail().unwrap_or("")
    )
}

fn invalid(operation: &str, detail: &str) -> MobileError {
    MobileError::Contract(
        DesktopError::new(
            ubm_core::contracts::BleErrorCode::ArgumentInvalid,
            ubm_core::contracts::BleErrorDomain::Core,
            operation,
        )
        .with_detail(detail.to_owned()),
    )
}

fn throw(env: &mut Env, message: &str) {
    let result = env.throw_new(JNIString::from(EXCEPTION_CLASS), JNIString::from(message));
    if result.is_err() && !env.exception_check() {
        let _ = env.throw_new(
            JNIString::from("java/lang/RuntimeException"),
            JNIString::from(message),
        );
    }
}

/// Every `Err` and panic becomes a typed `MobileCoreException`; the native
/// returns its default.
struct ThrowMobile;

impl<T: Default> ErrorPolicy<T, MobileError> for ThrowMobile {
    type Captures<'unowned_env_local: 'native_method, 'native_method> = &'static str;

    fn on_error<'unowned_env_local: 'native_method, 'native_method>(
        env: &mut Env<'unowned_env_local>,
        operation: &mut Self::Captures<'unowned_env_local, 'native_method>,
        err: MobileError,
    ) -> jni::errors::Result<T> {
        match err {
            MobileError::Contract(error) => throw(env, &wire_message(&error)),
            MobileError::Jni(error) => {
                eprintln!("ubm-mobile jni [{operation}]: {error:?}");
                throw(
                    env,
                    &format!("platform.failure|platform|{operation}|jni-call-failed"),
                );
            }
        }
        Ok(T::default())
    }

    fn on_panic<'unowned_env_local: 'native_method, 'native_method>(
        env: &mut Env<'unowned_env_local>,
        operation: &mut Self::Captures<'unowned_env_local, 'native_method>,
        _payload: Box<dyn std::any::Any + Send + 'static>,
    ) -> jni::errors::Result<T> {
        throw(
            env,
            &format!("lifecycle.invariant-violation|core|{operation}|rust-panic-contained"),
        );
        Ok(T::default())
    }
}

// -- Rust → Java request mapping ----------------------------------------

/// One typed argument of a `RadioHost` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Long(i64),
    Int(i32),
    Bool(bool),
    Str(Option<String>),
    Bytes(Vec<u8>),
    Strings(Vec<String>),
    Longs(Vec<i64>),
    OrderedOpenIdentity(ubm_mobile::OrderedOpenIdentity),
}

impl Arg {
    const fn descriptor(&self) -> &'static str {
        match self {
            Self::Long(_) => "J",
            Self::Int(_) => "I",
            Self::Bool(_) => "Z",
            Self::Str(_) => "Ljava/lang/String;",
            Self::Bytes(_) => "[B",
            Self::Strings(_) => "[Ljava/lang/String;",
            Self::Longs(_) => "[J",
            Self::OrderedOpenIdentity(_) => "Lcom/ubm/core/MobileCoreBridge$OrderedOpenIdentity;",
        }
    }
}

/// The JNI signature of a `void` method taking `args`.
#[must_use]
pub fn signature(args: &[Arg]) -> String {
    let mut out = String::from("(");
    for arg in args {
        out.push_str(arg.descriptor());
    }
    out.push_str(")V");
    out
}

fn id(value: u64) -> Arg {
    Arg::Long(i64::try_from(value).unwrap_or(i64::MAX))
}

/// Epochs are opaque u64 identities; JNI transports their exact signed bit pattern.
fn epoch_arg(value: u64) -> Arg {
    Arg::Long(i64::from_ne_bytes(value.to_ne_bytes()))
}

fn epoch_from_jlong(value: jlong) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

fn text(value: &str) -> Arg {
    Arg::Str(Some(value.to_owned()))
}

fn occurrence(value: u64) -> Arg {
    id(value)
}

fn instance_args(instance: &Instance) -> Vec<Arg> {
    vec![
        text(&instance.peer_id),
        text(&instance.service_uuid),
        occurrence(instance.service_occurrence),
        text(&instance.characteristic_uuid),
        occurrence(instance.characteristic_occurrence),
    ]
}

fn delivery(mode: Option<DeliveryMode>) -> Arg {
    Arg::Str(mode.map(|mode| mode.as_str().to_owned()))
}

/// The `RadioHost` method and arguments for one request.
#[must_use]
pub fn request_call(request: &RadioRequest) -> (&'static str, Vec<Arg>) {
    let rid = id(request.id());
    match request {
        RadioRequest::OwnerFence {
            owner_incarnation,
            retirement_id,
            ..
        } => (
            "ownerFence",
            vec![
                rid,
                text(&owner_incarnation.to_string()),
                text(&retirement_id.to_string()),
            ],
        ),
        RadioRequest::OwnerStatus {
            owner_incarnation,
            retirement_id,
            ..
        } => (
            "ownerStatus",
            vec![
                rid,
                text(&owner_incarnation.to_string()),
                text(&retirement_id.to_string()),
            ],
        ),
        RadioRequest::AdapterState { .. } => ("adapterState", vec![rid]),
        RadioRequest::StartScan { scan, .. } => {
            let android = scan.android.unwrap_or_default();
            (
                "startScan",
                vec![
                    rid,
                    Arg::Strings(scan.service_uuids.clone()),
                    Arg::Strings(scan.device_addresses.clone()),
                    Arg::Str(android.mode.map(|mode| mode.as_str().to_owned())),
                    Arg::Str(android.callback_type.map(|kind| kind.as_str().to_owned())),
                    Arg::Int(android.legacy.map_or(-1, i32::from)),
                    Arg::Long(android.report_delay_ms.map_or(0, i64::from)),
                    Arg::Str(android.phy.map(|phy| phy.as_str().to_owned())),
                ],
            )
        }
        RadioRequest::StopScan { .. } => ("stopScan", vec![rid]),
        RadioRequest::Connect {
            peer_id,
            auto_connect,
            preferred_phy,
            expected_generation,
            ..
        } => (
            "connect",
            vec![
                rid,
                text(peer_id),
                Arg::Bool(*auto_connect),
                Arg::Strings(
                    preferred_phy
                        .iter()
                        .map(|phy| phy.as_str().to_owned())
                        .collect(),
                ),
                text(expected_generation),
            ],
        ),
        RadioRequest::Disconnect { peer_id, .. } => ("disconnect", vec![rid, text(peer_id)]),
        RadioRequest::Discover { peer_id, .. } => ("discover", vec![rid, text(peer_id)]),
        RadioRequest::Read { instance, .. } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            ("read", args)
        }
        RadioRequest::Write {
            instance,
            value,
            with_response,
            ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            args.push(Arg::Bytes(value.clone()));
            args.push(Arg::Bool(*with_response));
            ("write", args)
        }
        RadioRequest::ReadDescriptor { descriptor, .. } => {
            let mut args = vec![rid];
            args.extend(instance_args(&descriptor.instance));
            args.push(text(&descriptor.descriptor_uuid));
            args.push(occurrence(descriptor.descriptor_occurrence));
            ("readDescriptor", args)
        }
        RadioRequest::WriteDescriptor {
            descriptor, value, ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(&descriptor.instance));
            args.push(text(&descriptor.descriptor_uuid));
            args.push(occurrence(descriptor.descriptor_occurrence));
            args.push(Arg::Bytes(value.clone()));
            ("writeDescriptor", args)
        }
        RadioRequest::EnableNotifications {
            instance,
            epoch,
            requested,
            preferred,
            ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            args.push(epoch_arg(*epoch));
            args.push(delivery(*requested));
            args.push(delivery(*preferred));
            ("enableNotifications", args)
        }
        RadioRequest::OrderedEnableNotifications {
            activation,
            requested,
            preferred,
            ..
        } => ordered_enable(rid, "enable", activation, *requested, *preferred),
        RadioRequest::OrderedDisableNotifications { activation, .. } => {
            ordered_enable(rid, "disable", activation, None, None)
        }
        RadioRequest::OrderedEnableSettlement { activation, .. } => {
            ordered_enable(rid, "settlement", activation, None, None)
        }
        RadioRequest::OrderedForgetEnableReceipt { activation, .. } => {
            ordered_enable(rid, "forget-receipt", activation, None, None)
        }
        RadioRequest::DisableNotifications {
            instance, epoch, ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            args.push(epoch_arg(*epoch));
            ("disableNotifications", args)
        }
        RadioRequest::OrderedOpen {
            group_id,
            members,
            item_capacity,
            byte_capacity,
            connection_generation,
            database_generation,
            snapshot_identity,
            ..
        } => (
            "orderedOpen",
            vec![
                rid,
                epoch_arg(*group_id),
                Arg::Longs(
                    members
                        .iter()
                        .map(|m| i64::from_ne_bytes(m.member_id.to_ne_bytes()))
                        .collect(),
                ),
                Arg::Strings(members.iter().map(|m| m.instance.peer_id.clone()).collect()),
                Arg::Strings(
                    members
                        .iter()
                        .map(|m| m.instance.service_uuid.clone())
                        .collect(),
                ),
                Arg::Longs(
                    members
                        .iter()
                        .map(|m| i64::from_ne_bytes(m.instance.service_occurrence.to_ne_bytes()))
                        .collect(),
                ),
                Arg::Strings(
                    members
                        .iter()
                        .map(|m| m.instance.characteristic_uuid.clone())
                        .collect(),
                ),
                Arg::Longs(
                    members
                        .iter()
                        .map(|m| {
                            i64::from_ne_bytes(m.instance.characteristic_occurrence.to_ne_bytes())
                        })
                        .collect(),
                ),
                Arg::Int(i32::from_ne_bytes(item_capacity.to_ne_bytes())),
                Arg::Int(i32::from_ne_bytes(byte_capacity.to_ne_bytes())),
                text(connection_generation),
                text(database_generation),
                text(&snapshot_identity.owner),
                epoch_arg(snapshot_identity.attachment),
                epoch_arg(snapshot_identity.revision),
            ],
        ),
        RadioRequest::OrderedOpenV2(request) => {
            use ubm_mobile::OrderedOpenRequestV2 as V2;
            let (method, identity) = match request {
                V2::Open { identity, .. } => ("orderedOpenV2", identity),
                V2::Recover { identity, .. } => ("orderedRecoverOpenV2", identity),
                V2::Acknowledge { identity, .. } => ("orderedAcknowledgeOpenV2", identity),
            };
            (
                method,
                vec![rid, Arg::OrderedOpenIdentity(identity.clone())],
            )
        }
        RadioRequest::OrderedRecoverOpen {
            group_id,
            open_request_id,
            peer_id,
            ..
        } => (
            "orderedRecoverOpen",
            vec![
                rid,
                epoch_arg(*group_id),
                epoch_arg(*open_request_id),
                text(peer_id),
            ],
        ),
        RadioRequest::OrderedAcknowledgeOpen {
            version,
            group_id,
            open_request_id,
            peer_id,
            ..
        } => (
            "orderedAcknowledgeOpen",
            vec![
                rid,
                Arg::Int(i32::from_ne_bytes(version.to_ne_bytes())),
                epoch_arg(*group_id),
                epoch_arg(*open_request_id),
                text(peer_id),
            ],
        ),
        RadioRequest::OrderedRegister {
            generation,
            group_id,
            member,
            ..
        } => ordered_request(
            rid,
            "register",
            generation,
            *group_id,
            member.member_id,
            member.source_member,
            member.epoch,
            0,
        ),
        RadioRequest::OrderedRetire {
            generation,
            group_id,
            member,
            ..
        } => ordered_request(
            rid,
            "retire",
            generation,
            *group_id,
            member.member_id,
            member.source_member,
            member.epoch,
            0,
        ),
        RadioRequest::OrderedDrain {
            generation,
            group_id,
            ..
        } => ordered_request(rid, "drain", generation, *group_id, 0, 0, 0, 0),
        RadioRequest::OrderedGraphPermitReserve { key, .. }
        | RadioRequest::OrderedGraphPermitReserveStatus { key, .. }
        | RadioRequest::OrderedGraphPermitReserveFence { key, .. }
        | RadioRequest::OrderedGraphPermitForgetRequest { key, .. } => {
            let operation = match request {
                RadioRequest::OrderedGraphPermitReserve { .. } => "reserve",
                RadioRequest::OrderedGraphPermitReserveStatus { .. } => "status",
                RadioRequest::OrderedGraphPermitReserveFence { .. } => "fence",
                _ => "forget",
            };
            (
                "orderedGraphPermitReservation",
                vec![
                    rid,
                    text(operation),
                    epoch_arg(key.authority_id),
                    epoch_arg(key.request_id),
                    text(&key.peer_id),
                    text(&key.connection_generation),
                    text(&key.identity.owner),
                    epoch_arg(key.identity.attachment),
                    epoch_arg(key.identity.revision),
                ],
            )
        }
        RadioRequest::OrderedGraphAuthorityReserveFence { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("reserve-fence"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityReserveForget { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("reserve-forget"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityReserveStatus { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("reserve-status"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityReserve { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("reserve"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityForgetOpen { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("forget-open"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityFenceOpen { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("fence-open"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityStatus { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("status"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityOpen { request, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("open"), epoch_arg(*request)],
        ),
        RadioRequest::OrderedGraphAuthorityClose { authority_id, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("close"), epoch_arg(*authority_id)],
        ),
        RadioRequest::OrderedGraphAuthorityForget { authority_id, .. } => (
            "orderedGraphAuthority",
            vec![rid, text("forget"), epoch_arg(*authority_id)],
        ),
        RadioRequest::OrderedGraphPermitAcquire { key, .. }
        | RadioRequest::OrderedGraphPermitRelease { key, .. }
        | RadioRequest::OrderedGraphPermitCommit { key, .. }
        | RadioRequest::OrderedGraphPermitForget { key, .. } => {
            let operation = if matches!(request, RadioRequest::OrderedGraphPermitAcquire { .. }) {
                "acquire"
            } else if matches!(request, RadioRequest::OrderedGraphPermitCommit { .. }) {
                "commit"
            } else if matches!(request, RadioRequest::OrderedGraphPermitForget { .. }) {
                "forget"
            } else {
                "release"
            };
            (
                "orderedGraphPermit",
                vec![
                    rid,
                    text(operation),
                    text(&key.peer_id),
                    text(&key.connection_generation),
                    text(&key.identity.owner),
                    epoch_arg(key.identity.attachment),
                    epoch_arg(key.identity.revision),
                    epoch_arg(key.authority_id),
                    epoch_arg(key.permit_id),
                ],
            )
        }
        RadioRequest::OrderedMarkerWrite {
            generation,
            group_id,
            marker_id,
            connection_generation,
            snapshot_identity,
            instance,
            value,
            with_response,
            ..
        } => {
            let (kind, generation) = match generation {
                ubm_mobile::OrderedSourceGeneration::Counter(value) => {
                    ("counter", value.to_string())
                }
                ubm_mobile::OrderedSourceGeneration::Opaque(value) => ("opaque", value.clone()),
            };
            let mut args = vec![
                rid,
                text("write"),
                text(kind),
                text(&generation),
                epoch_arg(*group_id),
                epoch_arg(*marker_id),
                text(connection_generation),
                text(&snapshot_identity.owner),
                epoch_arg(snapshot_identity.attachment),
                epoch_arg(snapshot_identity.revision),
            ];
            args.extend(instance_args(instance));
            args.push(Arg::Bytes(value.clone()));
            args.push(Arg::Bool(*with_response));
            ("orderedMarkerWrite", args)
        }
        RadioRequest::OrderedMarkerWriteSettlement {
            generation,
            group_id,
            marker_id,
            ..
        } => ordered_request(
            rid,
            "marker-write-settlement",
            generation,
            *group_id,
            0,
            0,
            0,
            *marker_id,
        ),
        RadioRequest::OrderedCancelMarkerWrite {
            generation,
            group_id,
            marker_id,
            ..
        } => ordered_request(
            rid,
            "cancel-marker-write",
            generation,
            *group_id,
            0,
            0,
            0,
            *marker_id,
        ),
        RadioRequest::OrderedForgetMarkerWriteReceipt {
            generation,
            group_id,
            marker_id,
            ..
        } => ordered_request(
            rid,
            "forget-marker-write-receipt",
            generation,
            *group_id,
            0,
            0,
            0,
            *marker_id,
        ),
        RadioRequest::OrderedAcknowledgeBatch {
            generation,
            group_id,
            batch_id,
            ..
        } => ordered_request(
            rid,
            "acknowledge-batch",
            generation,
            *group_id,
            0,
            0,
            0,
            *batch_id,
        ),
        RadioRequest::OrderedBarrier {
            generation,
            group_id,
            barrier_id,
            ..
        } => ordered_request(rid, "barrier", generation, *group_id, 0, 0, 0, *barrier_id),
        RadioRequest::OrderedClose {
            generation,
            group_id,
            ..
        } => ordered_request(rid, "close", generation, *group_id, 0, 0, 0, 0),
        RadioRequest::OrderedSeal {
            generation,
            group_id,
            ..
        } => ordered_request(rid, "seal", generation, *group_id, 0, 0, 0, 0),
        RadioRequest::OrderedRetainClaim {
            generation,
            group_id,
            member,
            ..
        } => ordered_request(
            rid,
            "retain-claim",
            generation,
            *group_id,
            member.member_id,
            member.source_member,
            member.epoch,
            0,
        ),
        RadioRequest::OrderedConfirmCleanup {
            generation,
            group_id,
            member,
            ..
        } => ordered_request(
            rid,
            "confirm-cleanup",
            generation,
            *group_id,
            member.member_id,
            member.source_member,
            member.epoch,
            0,
        ),
        RadioRequest::OrderedRemove {
            generation,
            group_id,
            ..
        } => ordered_request(rid, "remove", generation, *group_id, 0, 0, 0, 0),
        RadioRequest::OrderedForgetRemovalReceipt {
            generation,
            group_id,
            ..
        } => ordered_request(
            rid,
            "forget-removal-receipt",
            generation,
            *group_id,
            0,
            0,
            0,
            0,
        ),
        RadioRequest::ReadMtu { peer_id, .. } => ("readMtu", vec![rid, text(peer_id)]),
        RadioRequest::ReadWriteLimits { peer_id, .. } => {
            ("readWriteLimits", vec![rid, text(peer_id)])
        }
        RadioRequest::ReadWriteReadiness { peer_id, .. } => {
            ("readWriteReadiness", vec![rid, text(peer_id)])
        }
        RadioRequest::RequestMtu { peer_id, mtu, .. } => (
            "requestMtu",
            vec![rid, text(peer_id), Arg::Int(i32::from(*mtu))],
        ),
        RadioRequest::ReadRssi { peer_id, .. } => ("readRssi", vec![rid, text(peer_id)]),
        RadioRequest::RequestConnectionPriority {
            peer_id, priority, ..
        } => (
            "requestConnectionPriority",
            vec![rid, text(peer_id), text(priority.as_str())],
        ),
        RadioRequest::RequestSubrate { peer_id, mode, .. } => (
            "requestSubrate",
            vec![rid, text(peer_id), text(mode.as_str())],
        ),
        RadioRequest::ReadPhy { peer_id, .. } => ("readPhy", vec![rid, text(peer_id)]),
        RadioRequest::RequestPhy {
            peer_id, tx, rx, ..
        } => (
            "requestPhy",
            vec![
                rid,
                text(peer_id),
                Arg::Str(tx.map(|phy| phy.as_str().to_owned())),
                Arg::Str(rx.map(|phy| phy.as_str().to_owned())),
            ],
        ),
        RadioRequest::SecurityState { peer_id, .. } => ("securityState", vec![rid, text(peer_id)]),
        RadioRequest::CreateBond {
            peer_id, transport, ..
        } => (
            "createBond",
            vec![rid, text(peer_id), text(transport.as_str())],
        ),
        RadioRequest::CancelBond { peer_id, .. } => ("cancelBond", vec![rid, text(peer_id)]),
        RadioRequest::BondedPeers { .. } => ("bondedPeers", vec![rid]),
        RadioRequest::ResolvePeer { peer_id, .. } => ("resolvePeer", vec![rid, text(peer_id)]),
        RadioRequest::ConnectedPeers { services, .. } => {
            ("connectedPeers", vec![rid, Arg::Strings(services.clone())])
        }
        RadioRequest::AcquireBackground { kind, reason, .. } => (
            "acquireBackground",
            vec![rid, text(kind.as_str()), text(reason)],
        ),
        RadioRequest::ReleaseBackground { lease_id, .. } => {
            ("releaseBackground", vec![rid, text(lease_id)])
        }
        RadioRequest::UpdateBackgroundNotification {
            lease_id,
            title,
            body,
            ..
        } => (
            "updateBackgroundNotification",
            vec![rid, text(lease_id), text(title), Arg::Str(body.clone())],
        ),
        RadioRequest::AssociateCompanion {
            name,
            service_uuid,
            filters_json,
            ..
        } => (
            "associateCompanion",
            vec![
                rid,
                Arg::Str(name.clone()),
                Arg::Str(service_uuid.clone()),
                Arg::Str(filters_json.clone()),
            ],
        ),
        RadioRequest::ListCompanion { .. } => ("listCompanion", vec![rid]),
        RadioRequest::DisassociateCompanion { association_id, .. } => (
            "disassociateCompanion",
            vec![rid, Arg::Long(*association_id)],
        ),
        RadioRequest::ObservePresence { peer_id, .. } => {
            ("observePresence", vec![rid, text(peer_id)])
        }
        RadioRequest::StopPresence { peer_id, .. } => {
            ("unobservePresence", vec![rid, text(peer_id)])
        }
        RadioRequest::Close { .. } => ("close", vec![rid]),
    }
}

fn ordered_enable(
    rid: Arg,
    operation: &str,
    activation: &ubm_mobile::OrderedActivation,
    requested: Option<DeliveryMode>,
    preferred: Option<DeliveryMode>,
) -> (&'static str, Vec<Arg>) {
    let (kind, generation) = match &activation.source_generation {
        ubm_mobile::OrderedSourceGeneration::Counter(value) => ("counter", value.to_string()),
        ubm_mobile::OrderedSourceGeneration::Opaque(value) => ("opaque", value.clone()),
    };
    let mut args = vec![rid, text(operation)];
    let instance = &activation.instance;
    args.extend([
        text(&instance.peer_id),
        text(&instance.service_uuid),
        epoch_arg(instance.service_occurrence),
        text(&instance.characteristic_uuid),
        epoch_arg(instance.characteristic_occurrence),
        text(&activation.connection_generation),
        text(kind),
        text(&generation),
        epoch_arg(activation.group_id),
        epoch_arg(activation.member.member_id),
        epoch_arg(activation.member.source_member),
        epoch_arg(activation.member.epoch),
        epoch_arg(activation.activation_epoch),
        delivery(requested),
        delivery(preferred),
    ]);
    ("orderedEnable", args)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the frozen JNI ordered-request wire is positional and must preserve its Java argument layout"
)]
fn ordered_request(
    rid: Arg,
    operation: &str,
    generation: &ubm_mobile::OrderedSourceGeneration,
    group: u64,
    member: u64,
    source_member: u64,
    epoch: u64,
    batch: u64,
) -> (&'static str, Vec<Arg>) {
    let (kind, value) = match generation {
        ubm_mobile::OrderedSourceGeneration::Counter(value) => ("counter", value.to_string()),
        ubm_mobile::OrderedSourceGeneration::Opaque(value) => ("opaque", value.clone()),
    };
    (
        "orderedRequest",
        vec![
            rid,
            text(operation),
            text(kind),
            text(&value),
            epoch_arg(group),
            epoch_arg(member),
            epoch_arg(source_member),
            epoch_arg(epoch),
            epoch_arg(batch),
        ],
    )
}

// OrderedOpenIdentity's Java constructor takes, in order: version, owner UUID,
// slot, generation, original request, native group, peer, connection and
// database generations, snapshot owner/attachment/revision, parallel member
// id/peer/service/occurrence/characteristic/occurrence arrays, and capacities.
fn argument_objects<'a>(
    env: &mut Env<'a>,
    target: &JObject,
    args: &[Arg],
) -> jni::errors::Result<Vec<JObject<'a>>> {
    let mut objects: Vec<JObject> = Vec::with_capacity(args.len());
    for arg in args {
        let object = match arg {
            Arg::Str(Some(value)) => JObject::from(env.new_string(value)?),
            Arg::Bytes(value) => JObject::from(env.byte_array_from_slice(value)?),
            Arg::Longs(values) => {
                let array = env.new_long_array(values.len())?;
                array.set_region(env, 0, values)?;
                JObject::from(array)
            }
            Arg::Strings(values) => {
                let array = JObjectArray::<JString>::new(env, values.len(), JString::null())?;
                for (index, value) in values.iter().enumerate() {
                    let element = env.new_string(value)?;
                    array.set_element(env, index, &element)?;
                }
                JObject::from(array)
            }
            Arg::OrderedOpenIdentity(identity) => {
                let spec = &identity.spec;
                let members = &spec.members;
                let fields = vec![
                    Arg::Int(i32::from_ne_bytes(identity.version.to_ne_bytes())),
                    text(&identity.owner_incarnation.to_string()),
                    Arg::Int(i32::from_ne_bytes(identity.slot.to_ne_bytes())),
                    epoch_arg(identity.slot_generation),
                    epoch_arg(identity.original_request_id),
                    epoch_arg(identity.native_group_id),
                    text(&identity.peer_id),
                    text(&spec.connection_generation),
                    text(&spec.database_generation),
                    text(&spec.snapshot_identity.owner),
                    epoch_arg(spec.snapshot_identity.attachment),
                    epoch_arg(spec.snapshot_identity.revision),
                    Arg::Longs(
                        members
                            .iter()
                            .map(|m| i64::from_ne_bytes(m.member_id.to_ne_bytes()))
                            .collect(),
                    ),
                    Arg::Strings(members.iter().map(|m| m.instance.peer_id.clone()).collect()),
                    Arg::Strings(
                        members
                            .iter()
                            .map(|m| m.instance.service_uuid.clone())
                            .collect(),
                    ),
                    Arg::Longs(
                        members
                            .iter()
                            .map(|m| {
                                i64::from_ne_bytes(m.instance.service_occurrence.to_ne_bytes())
                            })
                            .collect(),
                    ),
                    Arg::Strings(
                        members
                            .iter()
                            .map(|m| m.instance.characteristic_uuid.clone())
                            .collect(),
                    ),
                    Arg::Longs(
                        members
                            .iter()
                            .map(|m| {
                                i64::from_ne_bytes(
                                    m.instance.characteristic_occurrence.to_ne_bytes(),
                                )
                            })
                            .collect(),
                    ),
                    Arg::Int(i32::from_ne_bytes(spec.item_capacity.to_ne_bytes())),
                    Arg::Int(i32::from_ne_bytes(spec.byte_capacity.to_ne_bytes())),
                ];
                let field_objects = argument_objects(env, target, &fields)?;
                let values = argument_values(&fields, &field_objects);
                let sig = RuntimeMethodSignature::from_str(signature(&fields))?;
                // Attached native threads cannot rely on FindClass using the app
                // loader. Resolve the descriptor through the host's loader.
                let host_class = env.get_object_class(target)?;
                let loader_sig = RuntimeMethodSignature::from_str("()Ljava/lang/ClassLoader;")?;
                let loader = env
                    .call_method(
                        &host_class,
                        JNIString::from("getClassLoader"),
                        MethodSignature::from(&loader_sig),
                        &[],
                    )?
                    .l()?;
                let name = JObject::from(
                    env.new_string("com.ubm.core.MobileCoreBridge$OrderedOpenIdentity")?,
                );
                let load_sig =
                    RuntimeMethodSignature::from_str("(Ljava/lang/String;)Ljava/lang/Class;")?;
                let loaded = env
                    .call_method(
                        &loader,
                        JNIString::from("loadClass"),
                        MethodSignature::from(&load_sig),
                        &[JValue::Object(&name)],
                    )?
                    .l()?;
                let class = env.cast_local::<JClass>(loaded)?;
                env.new_object(class, MethodSignature::from(&sig), &values)?
            }
            Arg::Str(None) | Arg::Long(_) | Arg::Int(_) | Arg::Bool(_) => JObject::null(),
        };
        objects.push(object);
    }
    Ok(objects)
}

fn argument_values<'a>(args: &'a [Arg], objects: &'a [JObject<'a>]) -> Vec<JValue<'a>> {
    args.iter()
        .zip(objects.iter())
        .map(|(arg, object)| match arg {
            Arg::Long(value) => JValue::Long(*value),
            Arg::Int(value) => JValue::Int(*value),
            Arg::Bool(value) => JValue::Bool(*value),
            Arg::Str(_)
            | Arg::Bytes(_)
            | Arg::Strings(_)
            | Arg::Longs(_)
            | Arg::OrderedOpenIdentity(_) => JValue::Object(object),
        })
        .collect()
}

fn call_host(
    env: &mut Env,
    target: &JObject,
    method: &str,
    args: &[Arg],
) -> jni::errors::Result<()> {
    let objects = argument_objects(env, target, args)?;
    let values = argument_values(args, &objects);
    let signature = RuntimeMethodSignature::from_str(signature(args))?;
    env.call_method(
        target,
        JNIString::from(method),
        MethodSignature::from(&signature),
        &values,
    )?;
    Ok(())
}

/// Counts host callbacks that failed (Java threw, or the JVM refused):
/// each also answers its request as a platform failure.
static CALLBACK_FAILURES: AtomicU64 = AtomicU64::new(0);

struct JniRadio {
    vm: JavaVM,
    host: Global<JObject<'static>>,
    owner: OnceLock<Weak<MobileHost>>,
    subrate_available: bool,
    admission_owner: ubm_mobile::OrderedAdmissionOwner,
    retirement_owner: Option<ubm_mobile::OrderedAdmissionOwner>,
    admission_version: u32,
    retirement_version: u32,
}

fn valid_request_id(id: u64) -> bool {
    id != 0 && i64::try_from(id).is_ok()
}

fn validate_dispatch(
    request: &RadioRequest,
    version: u32,
    owner: ubm_mobile::OrderedAdmissionOwner,
) -> Result<(), &'static str> {
    request.validate_owner_retirement(version, owner)?;
    if !valid_request_id(request.id()) {
        return Err("request id must be a positive JNI signed long");
    }
    let RadioRequest::OrderedOpenV2(operation) = request else {
        return Ok(());
    };
    use ubm_mobile::OrderedOpenRequestV2 as V2;
    let (attempt, identity, original) = match operation {
        V2::Open { id, identity } => (*id, identity, true),
        V2::Recover { id, identity } | V2::Acknowledge { id, identity } => (*id, identity, false),
    };
    if !matches!(
        version,
        ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION | ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION
    ) || identity.owner_incarnation != owner
    {
        return Err("v2 admission owner not negotiated");
    }
    identity.validate()?;
    if !valid_request_id(attempt) || !valid_request_id(identity.original_request_id) {
        return Err("request id exceeds JNI signed range");
    }
    if (attempt == identity.original_request_id) != original {
        return Err("invalid v2 attempt id");
    }
    Ok(())
}

impl JniRadio {
    fn invoke(&self, method: &str, args: &[Arg]) -> Result<(), JniError> {
        self.vm
            .attach_current_thread(|env| call_host(env, self.host.as_obj(), method, args))
    }
}

impl PlatformRadio for JniRadio {
    fn ordered_owner_retirement_version(&self) -> u32 {
        self.retirement_version
    }
    fn ordered_admission_owner(&self) -> Option<ubm_mobile::OrderedAdmissionOwner> {
        Some(self.admission_owner)
    }
    fn ordered_owner_retirement_owner(&self) -> Option<ubm_mobile::OrderedAdmissionOwner> {
        self.retirement_owner
    }
    fn ordered_admission_negotiation(&self) -> Option<ubm_mobile::OrderedAdmissionNegotiation> {
        Some(ubm_mobile::OrderedAdmissionNegotiation {
            version: self.admission_version,
            owner: self.admission_owner,
        })
    }
    fn connection_subrate_available(&self) -> bool {
        self.subrate_available
    }
    fn submit(&self, request: RadioRequest) {
        if let Err(detail) = validate_dispatch(
            &request,
            if matches!(
                request,
                RadioRequest::OwnerFence { .. } | RadioRequest::OwnerStatus { .. }
            ) {
                self.retirement_version
            } else {
                self.admission_version
            },
            if matches!(
                request,
                RadioRequest::OwnerFence { .. } | RadioRequest::OwnerStatus { .. }
            ) {
                self.retirement_owner.unwrap_or(self.admission_owner)
            } else {
                self.admission_owner
            },
        ) {
            if let Some(host) = self.owner.get().and_then(Weak::upgrade) {
                host.complete(
                    request.id(),
                    RadioCompletion::Failed(PlatformFailure::not_dispatched(
                        FailureKind::Platform,
                        detail,
                    )),
                );
            } else {
                eprintln!("ubm-mobile jni: invalid request before host existed: {detail}");
            }
            return;
        }
        let (method, args) = request_call(&request);
        if let Err(error) = self.invoke(method, &args) {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            let failure = PlatformFailure::new(
                FailureKind::Platform,
                format!("RadioHost.{method} failed: {error:?}"),
            );
            match self.owner.get().and_then(Weak::upgrade) {
                Some(host) => {
                    host.complete(request.id(), RadioCompletion::Failed(failure));
                }
                None => eprintln!("ubm-mobile jni: {method} failed before the host existed"),
            }
        }
    }

    fn cancel(&self, request_id: u64) {
        if !valid_request_id(request_id) {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: refusing invalid cancellation request id {request_id}");
            return;
        }
        if let Err(error) = self.invoke("cancel", &[id(request_id)]) {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: RadioHost.cancel({request_id}) failed: {error:?}");
        }
    }
}

struct JniWake {
    vm: JavaVM,
    listener: Global<JObject<'static>>,
}

impl WakeSink for JniWake {
    fn wake(&self, session_id: u64) {
        let result = self.vm.attach_current_thread(|env| {
            call_host(env, self.listener.as_obj(), "onWake", &[id(session_id)])
        });
        if let Err(error) = result {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: WakeListener.onWake({session_id}) failed: {error:?}");
        }
    }
}

// -- Java → Rust readers -------------------------------------------------

fn read_text(env: &mut Env, value: &JString, operation: &'static str) -> MobileResult<String> {
    read_opt_text(env, value, operation)?.ok_or_else(|| invalid(operation, "null string"))
}

fn read_opt_text(
    env: &mut Env,
    value: &JString,
    operation: &'static str,
) -> MobileResult<Option<String>> {
    if value.as_raw().is_null() {
        return Ok(None);
    }
    value
        .try_to_string(env)
        .map(Some)
        .map_err(|_| invalid(operation, "string is not valid UTF-8"))
}

fn read_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Vec<Option<String>>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let length = array.len(env)?;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let element = array.get_element(env, index)?;
        out.push(read_opt_text(env, &element, operation)?);
    }
    Ok(out)
}

fn read_required_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Vec<String>> {
    read_texts(env, array, operation)?
        .into_iter()
        .map(|entry| entry.ok_or_else(|| invalid(operation, "null array entry")))
        .collect()
}

/// A null array is "not reported"; an empty one is "reported, none".
fn read_opt_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Option<Vec<String>>> {
    if array.as_raw().is_null() {
        return Ok(None);
    }
    read_required_texts(env, array, operation).map(Some)
}

fn read_bytes(env: &mut Env, array: &JByteArray, operation: &'static str) -> MobileResult<Vec<u8>> {
    if array.as_raw().is_null() {
        return Err(invalid(operation, "null byte array"));
    }
    Ok(env.convert_byte_array(array)?)
}

fn read_byte_arrays(
    env: &mut Env,
    array: &JObjectArray<JByteArray>,
    operation: &'static str,
) -> MobileResult<Vec<Vec<u8>>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let length = array.len(env)?;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let element = array.get_element(env, index)?;
        out.push(read_bytes(env, &element, operation)?);
    }
    Ok(out)
}

fn read_ints(env: &mut Env, array: &JIntArray) -> MobileResult<Vec<i32>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![0; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn read_longs(env: &mut Env, array: &JLongArray) -> MobileResult<Vec<i64>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![0; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn read_bools(env: &mut Env, array: &JBooleanArray) -> MobileResult<Vec<bool>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![false; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn same_length(operation: &'static str, lengths: &[usize]) -> MobileResult<()> {
    if lengths.windows(2).all(|pair| pair[0] == pair[1]) {
        Ok(())
    } else {
        Err(invalid(operation, "parallel arrays differ in length"))
    }
}

fn to_u64(value: i64, operation: &'static str) -> MobileResult<u64> {
    u64::try_from(value).map_err(|_| invalid(operation, "negative occurrence or id"))
}

fn optional_int(value: jint) -> Option<i32> {
    (value != ABSENT_INT).then_some(value)
}

fn optional_i16(value: jint, operation: &'static str) -> MobileResult<Option<i16>> {
    optional_int(value)
        .map(|value| i16::try_from(value).map_err(|_| invalid(operation, "value out of range")))
        .transpose()
}

fn parse_enum<T>(
    value: &str,
    parse: fn(&str) -> Option<T>,
    operation: &'static str,
) -> MobileResult<T> {
    parse(value).ok_or_else(|| invalid(operation, "unknown enum value"))
}

fn adapter_snapshot(
    env: &mut Env,
    availability: &JString,
    authorization: &JString,
    power: &JString,
    safe_reason: &JString,
    operation: &'static str,
) -> MobileResult<AdapterSnapshot> {
    let availability = read_text(env, availability, operation)?;
    let authorization = read_text(env, authorization, operation)?;
    let power = read_text(env, power, operation)?;
    Ok(AdapterSnapshot {
        availability: parse_enum(&availability, AdapterAvailability::parse, operation)?,
        authorization: parse_enum(&authorization, AdapterAuthorization::parse, operation)?,
        power: parse_enum(&power, AdapterPower::parse, operation)?,
        safe_reason: read_opt_text(env, safe_reason, operation)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn security_state(
    env: &mut Env,
    bond: &JString,
    encryption: &JString,
    authentication: &JString,
    secure_connections: &JString,
    pairing_possible: jint,
    operation: &'static str,
) -> MobileResult<SecurityState> {
    let bond = read_text(env, bond, operation)?;
    let encryption = read_text(env, encryption, operation)?;
    let authentication = read_text(env, authentication, operation)?;
    let secure_connections = read_text(env, secure_connections, operation)?;
    Ok(SecurityState {
        bond: parse_enum(&bond, BondState::parse, operation)?,
        encryption: parse_enum(&encryption, EncryptionState::parse, operation)?,
        authentication: parse_enum(&authentication, AuthenticationState::parse, operation)?,
        secure_connections: parse_enum(
            &secure_connections,
            SecureConnectionsState::parse,
            operation,
        )?,
        pairing_possible: match pairing_possible {
            -1 => None,
            0 => Some(false),
            1 => Some(true),
            _ => return Err(invalid(operation, "pairingPossible must be -1, 0 or 1")),
        },
    })
}

/// Android `BluetoothGattCharacteristic.PROPERTY_*` bits → flags.
#[must_use]
pub fn android_properties(bits: i32) -> PropertyFlags {
    PropertyFlags {
        read: bits & 0x02 != 0,
        write_without_response: bits & 0x04 != 0,
        write: bits & 0x08 != 0,
        notify: bits & 0x10 != 0,
        indicate: bits & 0x20 != 0,
    }
}

/// Rebuild the discovery tree from its pre-order encoding.
pub fn discovery_tree(
    levels: &[i32],
    uuids: &[String],
    occurrences: &[i64],
    properties: &[i32],
) -> Result<Vec<ServiceSnapshot>, &'static str> {
    if levels.len() != uuids.len()
        || levels.len() != occurrences.len()
        || levels.len() != properties.len()
    {
        return Err("parallel arrays differ in length");
    }
    let mut services: Vec<ServiceSnapshot> = Vec::new();
    for index in 0..levels.len() {
        let occurrence = u64::try_from(occurrences[index]).map_err(|_| "negative occurrence")?;
        let uuid = uuids[index].clone();
        match levels[index] {
            0 => {
                if properties[index] & !7 != 0 {
                    return Err("invalid service metadata bits");
                }
                services.push(ServiceSnapshot {
                    primary: match properties[index] & 3 {
                        0 => None,
                        1 => Some(true),
                        2 => Some(false),
                        _ => return Err("invalid primary fact"),
                    },
                    included_services: if properties[index] & 4 != 0 {
                        Some(Vec::new())
                    } else {
                        None
                    },
                    uuid,
                    occurrence,
                    characteristics: Vec::new(),
                    access: std::default::Default::default(),
                });
            }
            1 => services
                .last_mut()
                .ok_or("characteristic before any service")?
                .characteristics
                .push(CharacteristicSnapshot {
                    uuid,
                    occurrence,
                    properties: android_properties(properties[index]),
                    descriptors: Vec::new(),
                }),
            2 => services
                .last_mut()
                .and_then(|service| service.characteristics.last_mut())
                .ok_or("descriptor before any characteristic")?
                .descriptors
                .push(DescriptorSnapshot { uuid, occurrence }),
            3 => services
                .last_mut()
                .ok_or("inclusion before any service")?
                .included_services
                .as_mut()
                .ok_or("inclusion without observed list")?
                .push(ubm_desktop::boundary::IncludedServiceReference { uuid, occurrence }),
            _ => return Err("level must be 0, 1, 2 or 3"),
        }
    }
    Ok(services)
}

fn completion_status(status: CompletionStatus) -> jint {
    match status {
        CompletionStatus::Delivered => 0,
        CompletionStatus::Late => 1,
        CompletionStatus::Mismatched => 2,
    }
}

fn ingress_status(status: IngressStatus) -> jint {
    match status {
        IngressStatus::Accepted => 0,
        IngressStatus::Dropped(IngressClass::Advertisement) => 1,
        IngressStatus::Dropped(IngressClass::Notification) => 2,
        IngressStatus::Dropped(IngressClass::Control) => 3,
        IngressStatus::ScanFailureRefused { closed: false, .. } => 3,
        IngressStatus::ScanFailureRefused { closed: true, .. } => 4,
        IngressStatus::Closed => 4,
    }
}

// This is a typed prefix-token stream, not the lossy public error projection:
// repeated metadata, null versus empty sequences and nested cleanup survive.
// Decimal strings preserve all 64 bits in JavaScript-facing JSON consumers.
fn scan_ingress_result(status: IngressStatus) -> serde_json::Value {
    use ubm_core::oversized_evidence::EvidenceToken;
    match status {
        IngressStatus::Accepted => serde_json::json!({"status":"accepted"}),
        IngressStatus::ScanFailureRefused { error, closed } => {
            let tokens: Vec<_> = ubm_desktop::errors::DesktopErrorEvidence::from(error)
                .into_tokens()
                .into_iter()
                .map(|token| match token {
                    EvidenceToken::Null => serde_json::json!(["null"]),
                    EvidenceToken::Bool(value) => serde_json::json!(["bool", value]),
                    EvidenceToken::I64(value) => serde_json::json!(["i64", value.to_string()]),
                    EvidenceToken::U64(value) => serde_json::json!(["u64", value.to_string()]),
                    EvidenceToken::F64Bits(value) => {
                        serde_json::json!(["f64-bits", value.to_string()])
                    }
                    EvidenceToken::Text(value) => serde_json::json!(["text", value]),
                    EvidenceToken::Bytes(value) => serde_json::json!([
                        "bytes-base64",
                        ubm_desktop::continuation_outbox::encode_base64(&value)
                    ]),
                    EvidenceToken::Map(value) => serde_json::json!(["map", value.to_string()]),
                    EvidenceToken::Array(value) => serde_json::json!(["array", value.to_string()]),
                })
                .collect();
            serde_json::json!({"status":"refused", "closed":closed,
                "representation":"ubm-desktop-error-tokens/1", "evidence":tokens})
        }
        IngressStatus::Dropped(class) => {
            serde_json::json!({"status":"dropped", "class":format!("{class:?}")})
        }
        IngressStatus::Closed => serde_json::json!({"status":"closed"}),
    }
}

#[cfg(ubm_jni_roundtrip)]
thread_local! {
    static TEST_RADIO: std::cell::RefCell<Option<ubm_mobile::ForeignRadio>> = const { std::cell::RefCell::new(None) };
}

fn complete(request_id: jlong, completion: RadioCompletion) -> jint {
    #[cfg(ubm_jni_roundtrip)]
    if let Some(status) = TEST_RADIO.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|radio| radio.complete(epoch_from_jlong(request_id), completion.clone()))
    }) {
        return completion_status(status);
    }

    match (legacy_host(), u64::try_from(request_id)) {
        (Some(host), Ok(request_id)) if valid_request_id(request_id) => {
            completion_status(host.complete(request_id, completion))
        }
        (Some(_), Ok(_)) => completion_status(CompletionStatus::Late),
        (Some(_), Err(_)) => completion_status(CompletionStatus::Late),
        (None, _) => STATUS_NO_HOST,
    }
}

fn ingest(ingress: RadioIngress) -> jint {
    match legacy_host() {
        Some(host) => ingress_status(host.ingest(ingress)),
        None => STATUS_NO_HOST,
    }
}

fn complete_for_binding(
    host: &MobileHost,
    request_id: jlong,
    built: MobileResult<RadioCompletion>,
) -> jint {
    let completion = built.unwrap_or_else(|error| {
        RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            format!("malformed completion from RadioHost: {error:?}"),
        ))
    });
    match u64::try_from(request_id) {
        Ok(id) if valid_request_id(id) => completion_status(host.complete(id, completion)),
        _ => completion_status(CompletionStatus::Late),
    }
}

/// A Java value that cannot be read answers the request as a platform
/// failure (the request never stays pending on a malformed answer).
fn complete_or_fail(request_id: jlong, built: MobileResult<RadioCompletion>) -> jint {
    let completion = built.unwrap_or_else(|error| {
        RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            format!("malformed completion from RadioHost: {error:?}"),
        ))
    });
    complete(request_id, completion)
}

fn instance(
    env: &mut Env,
    peer_id: &JString,
    service_uuid: &JString,
    service_occurrence: jlong,
    characteristic_uuid: &JString,
    characteristic_occurrence: jlong,
    operation: &'static str,
) -> MobileResult<Instance> {
    Ok(Instance {
        peer_id: read_text(env, peer_id, operation)?,
        service_uuid: read_text(env, service_uuid, operation)?,
        service_occurrence: to_u64(service_occurrence, operation)?,
        characteristic_uuid: read_text(env, characteristic_uuid, operation)?,
        characteristic_occurrence: to_u64(characteristic_occurrence, operation)?,
    })
}

fn contract_revision() -> &'static str {
    ubm_core::contracts::CONTRACT_REVISION
}

// -- natives: identity ------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeBuildIdentityJson<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env
                .new_string(ubm_build_identity_json(contract_revision()))?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.build-identity")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContractRevision<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env.new_string(contract_revision())?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.contract-revision")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeWireRevision<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env.new_string(ubm_mobile::WIRE_REVISION)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.wire-revision")
}

// -- natives: host ------------------------------------------------------

fn install_host<'caller>(
    env: &mut Env<'caller>,
    radio: JObject<'caller>,
    wake: JObject<'caller>,
    platform: JString<'caller>,
    owner: JString<'caller>,
    adapter_label: JString<'caller>,
    tagged: bool,
) -> MobileResult<jlong> {
    const OP: &str = "mobile.host.install";
    if radio.as_raw().is_null() || wake.as_raw().is_null() {
        return Err(invalid(OP, "radio and wake listener are required"));
    }
    let platform = match read_text(env, &platform, OP)?.as_str() {
        "android" => MobilePlatform::Android,
        "apple" => MobilePlatform::Apple,
        _ => return Err(invalid(OP, "platform must be android or apple")),
    };
    let owner = read_text(env, &owner, OP)?;
    let adapter_label = read_text(env, &adapter_label, OP)?;
    let installation = HostInstallation::reserve(tagged).map_err(|detail| {
        MobileError::Contract(
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                ubm_core::contracts::BleErrorDomain::Core,
                OP,
            )
            .with_detail(detail),
        )
    })?;
    let binding = installation.binding.unwrap_or(0);
    let mut retirement_version = 0;
    if tagged {
        let old_radios: Vec<_> = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binding_radios
            .values()
            .cloned()
            .collect();
        for old in old_radios {
            if env.is_same_object(&radio, old.as_obj())? {
                return Err(invalid(OP, "radio object already owns a retained binding"));
            }
        }
    }
    // The admission owner must be known before a binding token is disclosed,
    // including when the adapter does not implement owner retirement v4.
    let admission_version = ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION;
    let response = env
        .call_method(
            &radio,
            JNIString::from("orderedAdmissionOwner"),
            MethodSignature::from(&RuntimeMethodSignature::from_str("(I)Ljava/lang/String;")?),
            &[JValue::Int(
                jint::try_from(ubm_mobile::ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION)
                    .map_err(|_| invalid(OP, "invalid admission protocol version"))?,
            )],
        )?
        .l()?;
    let response = env.cast_local::<JString>(response)?;
    let response = read_opt_text(env, &response, OP)?.ok_or_else(|| {
        MobileError::Contract(
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                ubm_core::contracts::BleErrorDomain::Boundary,
                OP,
            )
            .with_detail("null ordered admission owner"),
        )
    })?;
    let admission_owner =
        ubm_mobile::OrderedAdmissionNegotiation::negotiate(admission_version, &response)
            .map_err(|detail| {
                let code = if response.is_empty() {
                    ubm_core::contracts::BleErrorCode::ProtocolIncompatible
                } else {
                    ubm_core::contracts::BleErrorCode::ProtocolMalformed
                };
                MobileError::Contract(
                    DesktopError::new(code, ubm_core::contracts::BleErrorDomain::Boundary, OP)
                        .with_detail(detail),
                )
            })?
            .owner;
    // v4 is independently negotiated: it owns the complete native retirement
    // fence and may therefore be a different incarnation from v2/v3 slots.
    // Negotiate it before any binding token is exposed, and do it for both
    // normal and retained-host entrypoints so production shutdown can use it.
    let value = env
        .call_method(
            &radio,
            JNIString::from("orderedAdmissionOwner"),
            MethodSignature::from(&RuntimeMethodSignature::from_str("(I)Ljava/lang/String;")?),
            &[JValue::Int(4)],
        )?
        .l()?;
    let value = env.cast_local::<JString>(value)?;
    let owner_v4 = read_opt_text(env, &value, OP)?
        .filter(|value| !value.is_empty())
        .map(|value| {
            ubm_mobile::OrderedAdmissionNegotiation::negotiate(4, &value)
                .map(|negotiated| negotiated.owner)
                .map_err(|detail| invalid(OP, detail))
        })
        .transpose()?;
    if owner_v4.is_some() {
        retirement_version = ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION;
    }
    if tagged {
        // Java may retain the token even if its callback throws or returns
        // false. Retain the exact adapter and owner before making that call.
        let retained = Arc::new(env.new_global_ref(&radio)?);
        let retirement = retirement_identity(binding)?;
        {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot
                .binding_owners
                .values()
                .any(|existing| *existing == admission_owner)
            {
                return Err(invalid(
                    OP,
                    "native owner already belongs to a retained binding",
                ));
            }
            slot.binding_radios.insert(binding, retained);
            slot.binding_owners.insert(binding, admission_owner);
            slot.unpublished.insert(
                binding,
                UnpublishedBinding {
                    owner: owner_v4,
                    retirement,
                    safe_to_retire: true,
                    fence_confirmed: false,
                    next_request: 0,
                    pending: None,
                },
            );
        }
        let accepted = env
            .call_method(
                &radio,
                JNIString::from("acceptBindingRoute"),
                MethodSignature::from(&RuntimeMethodSignature::from_str("(IJ)Z")?),
                &[JValue::Int(1), JValue::Long(binding)],
            )?
            .z()?;
        if !accepted {
            return Err(invalid(OP, "binding route version refused"));
        }
    }
    // The mobile constructor rejects this before touching its owner registry.
    // Keep this validation here so an unpublished adapter can be retired with
    // a native v4 receipt without asserting anything about a partial open.
    if tagged && owner.is_empty() {
        return Err(invalid(OP, "host owner is required"));
    }
    let jni_radio = Arc::new(JniRadio {
        vm: env.get_java_vm()?,
        host: env.new_global_ref(&radio)?,
        owner: OnceLock::new(),
        admission_owner,
        retirement_owner: owner_v4,
        admission_version,
        retirement_version,
        subrate_available: env
            .call_method(
                &radio,
                JNIString::from("subrateAvailable"),
                MethodSignature::from(&RuntimeMethodSignature::from_str("()Z")?),
                &[],
            )?
            .z()?,
    });
    let jni_wake = Arc::new(JniWake {
        vm: env.get_java_vm()?,
        listener: env.new_global_ref(&wake)?,
    });
    if tagged {
        host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unpublished
            .get_mut(&binding)
            .ok_or_else(|| invalid(OP, "reserved binding missing"))?
            .safe_to_retire = false;
    }
    let host = MobileHost::open_blocking_with_recording_registry(
        Arc::clone(&jni_radio) as Arc<dyn PlatformRadio>,
        jni_wake,
        HostOptions {
            platform,
            owner,
            adapter_label,
        },
        ubm_desktop::executor::desktop_runtime(),
        ubm_mobile::continuation::process_recording_registry(),
    )?;
    let host = Arc::new(host);
    #[cfg(ubm_jni_roundtrip)]
    if tagged {
        host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .test_radios
            .insert(binding, Arc::downgrade(&jni_radio));
    }
    let _ = jni_radio.owner.set(Arc::downgrade(&host));
    installation.publish(host);
    Ok(binding)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInstallHost<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    radio: JObject<'caller>,
    wake: JObject<'caller>,
    platform: JString<'caller>,
    owner: JString<'caller>,
    adapter_label: JString<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            install_host(env, radio, wake, platform, owner, adapter_label, false).map(|_| ())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.install")
}

const BINDING_ROUTE_VERSION: jint = 1;

/// Probe separately from the mobile wire revision: old native libraries do
/// not export this symbol and must remain on the legacy route.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeBindingRouteVersion(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
) -> jint {
    BINDING_ROUTE_VERSION
}

// This route is deliberately not advertised in the v3 wire revision. Both
// sides must opt in to version 1 before an adapter can use tagged callbacks.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInstallHostForBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    version: jint,
    radio: JObject<'caller>,
    wake: JObject<'caller>,
    platform: JString<'caller>,
    owner: JString<'caller>,
    adapter_label: JString<'caller>,
) -> jlong {
    unowned_env
        .with_env(|env| -> MobileResult<jlong> {
            if version != BINDING_ROUTE_VERSION {
                return Err(invalid(
                    "mobile.host.install",
                    "unsupported binding route version",
                ));
            }
            install_host(env, radio, wake, platform, owner, adapter_label, true)
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.install")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeHostInstalled<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> MobileResult<jboolean> { Ok(legacy_host().is_some()) })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.installed")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeShutdownHost<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            // The host stays installed while it shuts down: the platform's
            // answers to the teardown requests (Close) route through it.
            let (host, installing, tagged) = {
                let slot = host_slot()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (slot.host.clone(), slot.installing, !slot.binding_radios.is_empty())
            };
            let record = match host {
                Some(host) => {
                    let record = host.shutdown_blocking();
                    let mut slot = host_slot()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if ubm_mobile::binding_release_succeeded(&record)
                        && slot.host.as_ref().is_some_and(|current| Arc::ptr_eq(current, &host))
                    {
                        slot.host = None;
                    }
                    record
                }
                None if tagged => String::from("{\"failures\":[{\"code\":\"lifecycle.invalid-state\",\"detail\":\"tagged binding requires exact shutdown\",\"domain\":\"cleanup\",\"operation\":\"mobile.host.shutdown\",\"platform\":null,\"resourceKind\":\"host\"}],\"state\":\"release-failed\"}"),
                None if installing => String::from("{\"failures\":[{\"code\":\"lifecycle.invalid-state\",\"detail\":\"host installation is in progress\",\"domain\":\"cleanup\",\"operation\":\"mobile.host.shutdown\",\"platform\":null,\"resourceKind\":\"host\"}],\"state\":\"release-failed\"}"),
                None => String::from("{\"failures\":[],\"state\":\"released\"}"),
            };
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.shutdown")
}

struct BindingShutdown(i64);
impl Drop for BindingShutdown {
    fn drop(&mut self) {
        host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutting_down
            .remove(&self.0);
    }
}

const UNPUBLISHED_FAILURE: &str = r#"{"failures":[{"code":"lifecycle.invalid-state","detail":"unpublished binding remains retained; native retirement is unproved","domain":"cleanup","operation":"mobile.host.shutdown","platform":null,"resourceKind":"host"}],"state":"release-failed"}"#;
const BINDING_RELEASED: &str = r#"{"failures":[],"state":"released"}"#;

fn json_string(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn retire_unpublished(env: &mut Env, binding: i64) -> MobileResult<String> {
    let (radio, owner, retirement, request, method, rx) = {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(radio) = slot.binding_radios.get(&binding).cloned() else {
            return Ok(UNPUBLISHED_FAILURE.into());
        };
        let Some(entry) = slot.unpublished.get_mut(&binding) else {
            return Ok(UNPUBLISHED_FAILURE.into());
        };
        let Some(owner) = entry.owner.filter(|_| entry.safe_to_retire) else {
            return Ok(UNPUBLISHED_FAILURE.into());
        };
        let Some(request) = entry.next_request.checked_add(1) else {
            return Ok(UNPUBLISHED_FAILURE.into());
        };
        entry.next_request = request;
        // Repeat the same idempotent fence until its exact receipt arrives.
        // A failed call or lost reply does not prove that it reached Java.
        let method = if entry.fence_confirmed {
            "ownerStatus"
        } else {
            "ownerFence"
        };
        let (tx, rx) = mpsc::sync_channel(1);
        entry.pending = Some((request, tx));
        (radio, owner, entry.retirement, request, method, rx)
    };
    let dispatched = call_host(
        env,
        radio.as_obj(),
        method,
        &[
            Arg::Long(request),
            text(&owner.to_string()),
            text(&retirement.to_string()),
        ],
    );
    let result = if dispatched.is_ok() {
        rx.recv_timeout(Duration::from_millis(250)).ok()
    } else {
        None
    };
    let failure_detail = match &result {
        Some(RadioCompletion::Failed(failure)) => Some((
            "platform.failure",
            format!("native owner retirement failed: {failure:?}"),
        )),
        Some(RadioCompletion::OwnerRetirement(receipt))
            if receipt.owner_incarnation != owner || receipt.retirement_id != retirement =>
        {
            Some((
                "lifecycle.invalid-state",
                "native retirement receipt identity did not match".into(),
            ))
        }
        Some(RadioCompletion::OwnerRetirement(receipt))
            if !matches!(receipt.state, ubm_mobile::OwnerRetirementState::Quiescent) =>
        {
            Some((
                "lifecycle.invalid-state",
                format!(
                    "native owner retirement remains unresolved: {:?}",
                    receipt.state
                ),
            ))
        }
        None => Some((
            "lifecycle.invalid-state",
            "native owner retirement reply timed out".into(),
        )),
        _ => None,
    };
    let released = matches!(result, Some(RadioCompletion::OwnerRetirement(ref receipt))
        if receipt.owner_incarnation == owner && receipt.retirement_id == retirement
        && matches!(receipt.state, ubm_mobile::OwnerRetirementState::Quiescent));
    let fenced = matches!(result, Some(RadioCompletion::OwnerRetirement(ref receipt))
        if receipt.owner_incarnation == owner && receipt.retirement_id == retirement
        && matches!(receipt.state, ubm_mobile::OwnerRetirementState::FencedPending { .. }));
    let removed = {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = slot.unpublished.get_mut(&binding) {
            entry.pending = None;
            entry.fence_confirmed |= fenced;
        }
        if released {
            slot.unpublished.remove(&binding);
            slot.binding_owners.remove(&binding);
            slot.scan_recovery.remove(&binding);
            slot.scan_recovery_epochs.remove(&binding);
            slot.terminal_evidence_recovery.remove(&binding);
            slot.terminal_evidence_recovery_epochs.remove(&binding);
            remember_retirement(&mut slot, binding, BINDING_RELEASED.into());
            slot.binding_radios.remove(&binding)
        } else {
            None
        }
    };
    drop(removed);
    dispatched?;
    if let Some((code, detail)) = failure_detail {
        return Ok(format!(
            r#"{{"failures":[{{"code":"{code}","detail":{},"domain":"cleanup","operation":"mobile.host.shutdown","platform":null,"resourceKind":"host"}}],"state":"release-failed"}}"#,
            json_string(&detail)
        ));
    }
    Ok(if released {
        BINDING_RELEASED
    } else {
        UNPUBLISHED_FAILURE
    }
    .into())
}

// Shutdown identifies the owner before executing any cleanup. Failed cleanup
// keeps the exact entry available for late callbacks and future retries.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeShutdownHostForBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        let host = {
            let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((_, record)) = slot.retired.iter().find(|(id, _)| *id == binding) {
                return Ok(env.new_string(record)?.into_raw());
            }
            if slot.installing || (!slot.bindings.contains_key(&binding) && !slot.unpublished.contains_key(&binding)) {
                return Ok(env.new_string(UNPUBLISHED_FAILURE)?.into_raw());
            }
            if !slot.shutting_down.insert(binding) {
                return Ok(env.new_string(r#"{"failures":[{"code":"lifecycle.invalid-state","detail":"binding shutdown in progress","domain":"cleanup","operation":"mobile.host.shutdown","platform":null,"resourceKind":"host"}],"state":"release-failed"}"#)?.into_raw());
            }
            slot.bindings.get(&binding).cloned()
        };
        let _shutdown = BindingShutdown(binding);
        let Some(host) = host else {
            let record = retire_unpublished(env, binding)?;
            return Ok(env.new_string(record)?.into_raw());
        };
        let record = host.shutdown_blocking();
        if ubm_mobile::binding_release_succeeded(&record) {
            let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.bindings.get(&binding).is_some_and(|entry| Arc::ptr_eq(entry, &host)) {
                slot.bindings.remove(&binding);
                #[cfg(ubm_jni_roundtrip)]
                slot.test_radios.remove(&binding);
                remember_retirement(&mut slot, binding, record.clone());
                slot.binding_radios.remove(&binding);
                slot.binding_owners.remove(&binding);
                slot.scan_recovery.remove(&binding);
                slot.scan_recovery_epochs.remove(&binding);
                slot.terminal_evidence_recovery.remove(&binding);
                slot.terminal_evidence_recovery_epochs.remove(&binding);
            }
        }
        Ok(env.new_string(record)?.into_raw())
    }).resolve_with::<ThrowMobile, _>(|| "mobile.host.shutdown")
}

// -- natives: sessions --------------------------------------------------

fn require_host(operation: &'static str) -> MobileResult<MobileHost> {
    require_legacy_session_host(operation)
}

fn legacy_session_host() -> (Option<MobileHost>, bool) {
    let slot = host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (
        slot.host.as_ref().map(|host| host.as_ref().clone()),
        !slot.binding_radios.is_empty(),
    )
}

fn require_legacy_session_host(operation: &'static str) -> MobileResult<MobileHost> {
    let (host, tagged) = legacy_session_host();
    if let Some(host) = host {
        return Ok(host);
    }
    if tagged {
        return Err(invalid(operation, "tagged session requires exact binding"));
    }
    Err(MobileError::Contract(
        DesktopError::new(
            ubm_core::contracts::BleErrorCode::LifecycleDestroyed,
            ubm_core::contracts::BleErrorDomain::Core,
            operation,
        )
        .with_detail("no mobile host is installed"),
    ))
}

fn require_session(
    session_id: jlong,
    operation: &'static str,
) -> MobileResult<ubm_mobile::MobileSession> {
    let host = require_legacy_session_host(operation)?;
    session_from_host(&host, session_id, operation)
}

fn session_from_host(
    host: &MobileHost,
    session_id: jlong,
    operation: &'static str,
) -> MobileResult<ubm_mobile::MobileSession> {
    u64::try_from(session_id)
        .ok()
        .and_then(|id| host.session(id))
        .ok_or_else(|| {
            MobileError::Contract(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleDestroyed,
                    ubm_core::contracts::BleErrorDomain::Core,
                    operation,
                )
                .with_detail("unknown or disposed session"),
            )
        })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOpenSession<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    owner: JString<'caller>,
    expected_wire_revision: JString<'caller>,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.open";
            let owner = read_text(env, &owner, OP)?;
            let expected = read_text(env, &expected_wire_revision, OP)?;
            let scope = read_text(env, &background_scope, OP)?;
            let host = require_legacy_session_host(OP)?;
            let (_, record) = host.admit_scoped_session(
                &owner,
                &scope,
                &expected,
                &ubm_build_identity_json(contract_revision()),
            )?;
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.open")
}

/// Ends one background scope (React Native module invalidation): releases
/// every foreground-service lease its sessions acquired. Returns the
/// cleanup record JSON; with no host installed nothing is held.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeReleaseBackgroundScope<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.background.release-scope";
            let scope = read_text(env, &background_scope, OP)?;
            let (host, tagged) = legacy_session_host();
            let record = match host {
                Some(host) => host.release_background_scope_blocking(&scope),
                None if tagged => return Err(invalid(OP, "tagged scope requires exact binding")),
                None => String::from("{\"failures\":[],\"state\":\"released\"}"),
            };
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.background.release-scope")
}

struct JniCallback {
    vm: JavaVM,
    callback: Global<JObject<'static>>,
}

impl JniCallback {
    fn deliver(self, envelope: String) {
        let result = self
            .vm
            .attach_current_thread(|env| -> jni::errors::Result<()> {
                let text = env.new_string(&envelope)?;
                let signature = RuntimeMethodSignature::from_str("(Ljava/lang/String;)V")?;
                env.call_method(
                    self.callback.as_obj(),
                    JNIString::from("onResult"),
                    MethodSignature::from(&signature),
                    &[JValue::Object(&text)],
                )?;
                Ok(())
            });
        if let Err(error) = result {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: InvokeCallback.onResult failed: {error:?}");
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInvoke<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    op: JString<'caller>,
    args_json: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.session.invoke";
            if callback.as_raw().is_null() {
                return Err(invalid(OP, "callback is required"));
            }
            let session = require_session(session_id, OP)?;
            let op = read_text(env, &op, OP)?;
            let args = read_text(env, &args_json, OP)?;
            let callback = JniCallback {
                vm: env.get_java_vm()?,
                callback: env.new_global_ref(&callback)?,
            };
            session.invoke(
                &op,
                &args,
                Box::new(move |envelope| callback.deliver(envelope)),
            );
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.invoke")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeNegotiateScanEndVersion<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    offered: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.scan-end-version";
            let session = require_session(session_id, OP)?;
            let offered = read_text(env, &offered, OP)?;
            Ok(env
                .new_string(session.negotiate_scan_end_version(&offered)?)?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.scan-end-version")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeNegotiateScanEndVersionForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    offered: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.scan-end-version";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let session = session_from_host(&host, session_id, OP)?;
            let offered = read_text(env, &offered, OP)?;
            Ok(env
                .new_string(session.negotiate_scan_end_version(&offered)?)?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.scan-end-version")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeNegotiateScanEndVersionQualified<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    offered: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.scan-end-version";
            let session = require_session(session_id, OP)?;
            let offered = read_text(env, &offered, OP)?;
            Ok(env
                .new_string(session.negotiate_scan_end_version_with_max(&offered, 1)?)?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.scan-end-version")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeNegotiateScanEndVersionQualifiedForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    offered: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.scan-end-version";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let session = session_from_host(&host, session_id, OP)?;
            let offered = read_text(env, &offered, OP)?;
            Ok(env
                .new_string(session.negotiate_scan_end_version_with_max(&offered, 1)?)?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.scan-end-version")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeDrain<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    max_items: jint,
    max_bytes: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.drain";
            let session = require_session(session_id, OP)?;
            let max_items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let max_bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            Ok(env
                .new_string(session.drain(max_items, max_bytes))?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.drain")
}

// Identity lookup is available even if acceptBindingRoute refused or threw.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeFindRetainedBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    radio: JObject<'caller>,
) -> jlong {
    unowned_env
        .with_env(|env| -> MobileResult<jlong> {
            if radio.as_raw().is_null() {
                return Err(invalid("mobile.host.find-binding", "radio is required"));
            }
            let radios: Vec<_> = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .binding_radios
                .iter()
                .map(|(id, radio)| (*id, Arc::clone(radio)))
                .collect();
            for (id, retained) in radios {
                if env.is_same_object(&radio, retained.as_obj())? {
                    return Ok(id);
                }
            }
            Ok(0)
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.find-binding")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOpenSessionForBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner: JString<'caller>,
    expected_wire_revision: JString<'caller>,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.open";
            let owner = read_text(env, &owner, OP)?;
            let expected = read_text(env, &expected_wire_revision, OP)?;
            let scope = read_text(env, &background_scope, OP)?;
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let (_, record) = host.admit_scoped_session(
                &owner,
                &scope,
                &expected,
                &ubm_build_identity_json(contract_revision()),
            )?;
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.open")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeReleaseBackgroundScopeForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.background.release-scope";
            let scope = read_text(env, &background_scope, OP)?;
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let record = host.release_background_scope_blocking(&scope);
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.background.release-scope")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInvokeForBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    op: JString<'caller>,
    args_json: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.session.invoke";
            if callback.as_raw().is_null() {
                return Err(invalid(OP, "callback is required"));
            }
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let session = session_from_host(&host, session_id, OP)?;
            let op = read_text(env, &op, OP)?;
            let args = read_text(env, &args_json, OP)?;
            let callback = JniCallback {
                vm: env.get_java_vm()?,
                callback: env.new_global_ref(&callback)?,
            };
            session.invoke(
                &op,
                &args,
                Box::new(move |envelope| callback.deliver(envelope)),
            );
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.invoke")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeDrainForBinding<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    max_items: jint,
    max_bytes: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.drain";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let session = session_from_host(&host, session_id, OP)?;
            let max_items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let max_bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            Ok(env
                .new_string(session.drain(max_items, max_bytes))?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.drain")
}

// -- natives: completions -----------------------------------------------

fn continuation_callback(
    env: &mut Env,
    callback: &JObject,
    operation: &'static str,
) -> MobileResult<ubm_mobile::Completion> {
    if callback.as_raw().is_null() {
        return Err(invalid(operation, "callback is required"));
    }
    let callback = JniCallback {
        vm: env.get_java_vm()?,
        callback: env.new_global_ref(callback)?,
    };
    Ok(Box::new(move |envelope| callback.deliver(envelope)))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationDescribeBacklog<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.backlog";
            let callback = continuation_callback(env, &callback, OP)?;
            require_host(OP)?.continuation_describe_backlog(callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.backlog")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationDeclarationReplacementFailure<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    declaration: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.continuation.declaration";
            let declaration = read_text(env, &declaration, OP)?;
            match require_host(OP)?.continuation_declaration_replacement_failure(&declaration) {
                Some(reason) => Ok(env.new_string(reason)?.into_raw()),
                None => Ok(std::ptr::null_mut()),
            }
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.declaration")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationExecute<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    peer_id: JString<'caller>,
    declaration: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.execute";
            let peer = read_text(env, &peer_id, OP)?;
            let declaration = read_text(env, &declaration, OP)?;
            let callback = continuation_callback(env, &callback, OP)?;
            require_host(OP)?.continuation_execute(&peer, &declaration, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.execute")
}

macro_rules! continuation_declaration_entry {
    ($entry:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $entry<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            argument: JString<'caller>,
        ) -> jstring {
            unowned_env
                .with_env(|env| -> MobileResult<jstring> {
                    const OP: &str = "mobile.continuation.declaration";
                    let argument = read_text(env, &argument, OP)?;
                    let answer = require_host(OP)?.$method(&argument);
                    Ok(env.new_string(answer)?.into_raw())
                })
                .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.declaration")
        }
    };
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationConfigureRecordingDirectory<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    path: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.continuation.recording.configure";
            let path = read_text(env, &path, OP)?;
            let answer = ubm_desktop::continuation::envelope(
                ubm_mobile::continuation::process_recording_registry()
                    .configure_directory(std::path::Path::new(&path))
                    .map_err(ubm_desktop::continuation::recording_failure),
            );
            Ok(env.new_string(answer)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.configure")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeConfigureTerminalEvidenceMaximumBytes<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    maximum_bytes: jlong,
) {
    unowned_env
        .with_env(|_| -> MobileResult<()> {
            let maximum_bytes = u64::try_from(maximum_bytes).map_err(|_| {
                invalid(
                    "mobile.terminal-evidence.configure",
                    "maximum must be positive",
                )
            })?;
            ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(maximum_bytes)
                .map_err(|detail| invalid("mobile.terminal-evidence.configure", detail))?;
            // The ordinary RN manager may already have installed the one process
            // host. Attach the same process-selected ledger before returning so a
            // later terminal-evidence manager never observes a capability gap.
            if let Some(host) = legacy_host() {
                host.configure_terminal_evidence_maximum_bytes(maximum_bytes)
                    .map_err(MobileError::Contract)?;
            }
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.terminal-evidence.configure")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationRecordingControl<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    operation: JString<'caller>,
    id: JString<'caller>,
    token: JString<'caller>,
    max_items: jint,
    max_bytes: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.continuation.recording";
            let operation = read_text(env, &operation, OP)?;
            let id = read_text(env, &id, OP)?;
            let token = read_text(env, &token, OP)?;
            let items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            // Keep the registry locked through stop admission and execution so
            // installation cannot reserve ownership between lookup and stop.
            let guard = if operation == "stop" {
                Some(
                    host_slot()
                        .lock()
                        .map_err(|_| invalid(OP, "host registry interrupted"))?,
                )
            } else {
                None
            };
            let (host, tagged) = if let Some(slot) = &guard {
                let permitted = if slot.host.is_none() {
                    recording_stop_permitted(slot, None)
                } else {
                    !slot.installing && slot.binding_radios.is_empty() && slot.bindings.is_empty()
                };
                if !permitted {
                    return Ok(env.new_string(recording_stop_owner_conflict())?.into_raw());
                }
                (slot.host.as_ref().map(|host| host.as_ref().clone()), false)
            } else {
                legacy_session_host()
            };
            let answer = if let Some(host) = host {
                host.continuation_recording_control(&operation, &id, &token, items, bytes)
            } else if tagged {
                return Err(invalid(
                    OP,
                    "tagged recording control requires exact binding",
                ));
            } else {
                ubm_mobile::continuation::recording_control(
                    &ubm_mobile::continuation::process_recording_registry(),
                    None,
                    &operation,
                    &id,
                    &token,
                    items,
                    bytes,
                )
            };
            drop(guard);
            Ok(env.new_string(answer)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording")
}

/// Private journal access with no host selection. Ordinary recorded routes are
/// not yet indexed by the journal authority, so retain the conservative stop
/// gate while any host (including an unpublished binding) may own work.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationRecordingControlOffline<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    operation: JString<'caller>,
    id: JString<'caller>,
    token: JString<'caller>,
    max_items: jint,
    max_bytes: jint,
) -> jstring {
    env.with_env(|env| -> MobileResult<jstring> {
        const OP: &str = "mobile.continuation.recording";
        let operation = read_text(env, &operation, OP)?;
        let id = read_text(env, &id, OP)?;
        let token = read_text(env, &token, OP)?;
        let items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
        let bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
        let guard = if operation == "stop" {
            Some(
                host_slot()
                    .lock()
                    .map_err(|_| invalid(OP, "host registry interrupted"))?,
            )
        } else {
            None
        };
        let answer = if guard
            .as_ref()
            .is_some_and(|slot| !recording_stop_permitted(slot, None))
        {
            recording_stop_owner_conflict()
        } else {
            ubm_mobile::continuation::recording_control(
                &ubm_mobile::continuation::process_recording_registry(),
                None,
                &operation,
                &id,
                &token,
                items,
                bytes,
            )
        };
        Ok(env.new_string(answer)?.into_raw())
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOrderedRecordingReceiptControl<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    operation: JString<'caller>,
    id: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.recording.receipt";
            let operation = read_text(env, &operation, OP)?;
            let id = read_text(env, &id, OP)?;
            let completion = continuation_callback(env, &callback, OP)?;
            let session = require_session(session_id, OP)?;
            session.ordered_recording_receipt_control(operation, id, completion);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.receipt")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOrderedRecordingVersion<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jint {
    // Only the explicitly enabled debug harness advertises this synthetic
    // path. It does not establish native graph-pin or reclaim qualification.
    #[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
    if legacy_host().is_some_and(|host| host.synthetic_marker_enabled_for_test()) {
        return 1;
    }
    0
}

/// No live owner exists on this route, including in synthetic test builds.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOrderedRecordingVersionOffline<
    'caller,
>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jint {
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationOrderedOffline<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    operation: JString<'caller>,
    id: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.recording.ordered-context-offline";
            let operation = read_text(env, &operation, OP)?;
            let id = read_text(env, &id, OP)?;
            let completion = continuation_callback(env, &callback, OP)?;
            ubm_mobile::continuation::recording_ordered_offline(operation, id, completion);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.ordered-context-offline")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationStartOrdered<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    handle: JString<'caller>,
    recording_id: JString<'caller>,
    max_bytes: JString<'caller>,
    max_records: JString<'caller>,
    declaration: JString<'caller>,
    budget_ms: jlong,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.recording.start-ordered";
            let handle = read_text(env, &handle, OP)?;
            let recording_id = read_text(env, &recording_id, OP)?;
            let max_bytes = read_text(env, &max_bytes, OP)?;
            let max_records = read_text(env, &max_records, OP)?;
            let declaration = read_text(env, &declaration, OP)?;
            let budget_ms = u64::try_from(budget_ms).map_err(|_| invalid(OP, "budgetMs"))?;
            let completion = continuation_callback(env, &callback, OP)?;
            let session = require_session(session_id, OP)?;
            session.start_ordered_recording_wire(
                handle,
                recording_id,
                max_bytes,
                max_records,
                declaration,
                budget_ms,
                completion,
            );
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.start-ordered")
}

macro_rules! ordered_recording_control_entry {
    ($name:ident, $context:expr) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            session_id: jlong,
            id: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            unowned_env
                .with_env(|env| -> MobileResult<()> {
                    const OP: &str = "mobile.continuation.recording.ordered";
                    let id = read_text(env, &id, OP)?;
                    let completion = continuation_callback(env, &callback, OP)?;
                    let session = require_session(session_id, OP)?;
                    session.ordered_recording_control_wire(id, $context, completion);
                    Ok(())
                })
                .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.ordered")
        }
    };
}
ordered_recording_control_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationStopOrdered,
    false
);
ordered_recording_control_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationOrderedContext,
    true
);

continuation_declaration_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationReserveDeclaration,
    continuation_reserve_declaration
);
continuation_declaration_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationCommitDeclaration,
    continuation_commit_declaration
);
continuation_declaration_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationCancelDeclaration,
    continuation_cancel_declaration
);
continuation_declaration_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationSeedDeclaration,
    continuation_seed_declaration
);

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationPrepareClaim<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    max_items: jint,
    max_bytes: jint,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.prepare";
            let items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            if items == 0 || bytes == 0 {
                return Err(invalid(OP, "claim bounds must be positive"));
            }
            let callback = continuation_callback(env, &callback, OP)?;
            require_host(OP)?.continuation_prepare_claim(items, bytes, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.prepare")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaim<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    token: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.acknowledge";
            let token = read_text(env, &token, OP)?;
            let callback = continuation_callback(env, &callback, OP)?;
            require_host(OP)?.continuation_acknowledge_claim(&token, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.acknowledge")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationDescribeBacklogForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.backlog";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let callback = continuation_callback(env, &callback, OP)?;
            host.continuation_describe_backlog(callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.backlog")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationDeclarationReplacementFailureForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    declaration: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.continuation.declaration";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let declaration = read_text(env, &declaration, OP)?;
            match host.continuation_declaration_replacement_failure(&declaration) {
                Some(reason) => Ok(env.new_string(reason)?.into_raw()),
                None => Ok(std::ptr::null_mut()),
            }
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.declaration")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationExecuteForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    peer_id: JString<'caller>,
    declaration: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.execute";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let peer = read_text(env, &peer_id, OP)?;
            let declaration = read_text(env, &declaration, OP)?;
            let callback = continuation_callback(env, &callback, OP)?;
            host.continuation_execute(&peer, &declaration, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.execute")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOrderedRecordingReceiptControlForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    operation: JString<'caller>,
    id: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.recording.receipt";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let operation = read_text(env, &operation, OP)?;
            let id = read_text(env, &id, OP)?;
            let completion = continuation_callback(env, &callback, OP)?;
            let session = session_from_host(&host, session_id, OP)?;
            session.ordered_recording_receipt_control(operation, id, completion);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.receipt")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationStartOrderedForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    session_id: jlong,
    handle: JString<'caller>,
    recording_id: JString<'caller>,
    max_bytes: JString<'caller>,
    max_records: JString<'caller>,
    declaration: JString<'caller>,
    budget_ms: jlong,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.recording.start-ordered";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let handle = read_text(env, &handle, OP)?;
            let recording_id = read_text(env, &recording_id, OP)?;
            let max_bytes = read_text(env, &max_bytes, OP)?;
            let max_records = read_text(env, &max_records, OP)?;
            let declaration = read_text(env, &declaration, OP)?;
            let budget_ms = u64::try_from(budget_ms).map_err(|_| invalid(OP, "budgetMs"))?;
            let completion = continuation_callback(env, &callback, OP)?;
            let session = session_from_host(&host, session_id, OP)?;
            session.start_ordered_recording_wire(
                handle,
                recording_id,
                max_bytes,
                max_records,
                declaration,
                budget_ms,
                completion,
            );
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.start-ordered")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationPrepareClaimForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    max_items: jint,
    max_bytes: jint,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.prepare";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            if items == 0 || bytes == 0 {
                return Err(invalid(OP, "claim bounds must be positive"));
            }
            let callback = continuation_callback(env, &callback, OP)?;
            host.continuation_prepare_claim(items, bytes, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.prepare")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaimForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    token: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.continuation.acknowledge";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let token = read_text(env, &token, OP)?;
            let callback = continuation_callback(env, &callback, OP)?;
            host.continuation_acknowledge_claim(&token, callback);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.acknowledge")
}

macro_rules! ordered_recording_control_binding_entry {
    ($name:ident, $context:expr) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            session_id: jlong,
            id: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            unowned_env
                .with_env(|env| -> MobileResult<()> {
                    const OP: &str = "mobile.continuation.recording.ordered";
                    let host =
                        binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
                    let session = session_from_host(&host, session_id, OP)?;
                    let id = read_text(env, &id, OP)?;
                    let completion = continuation_callback(env, &callback, OP)?;
                    session.ordered_recording_control_wire(id, $context, completion);
                    Ok(())
                })
                .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.ordered")
        }
    };
}
ordered_recording_control_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationStopOrderedForBinding,
    false
);
ordered_recording_control_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationOrderedContextForBinding,
    true
);

macro_rules! continuation_declaration_binding_entry {
    ($entry:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $entry<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            argument: JString<'caller>,
        ) -> jstring {
            unowned_env
                .with_env(|env| -> MobileResult<jstring> {
                    const OP: &str = "mobile.continuation.declaration";
                    let host =
                        binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
                    let argument = read_text(env, &argument, OP)?;
                    Ok(env.new_string(host.$method(&argument))?.into_raw())
                })
                .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.declaration")
        }
    };
}
continuation_declaration_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationReserveDeclarationForBinding,
    continuation_reserve_declaration
);
continuation_declaration_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationCommitDeclarationForBinding,
    continuation_commit_declaration
);
continuation_declaration_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationCancelDeclarationForBinding,
    continuation_cancel_declaration
);
continuation_declaration_binding_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationSeedDeclarationForBinding,
    continuation_seed_declaration
);

// Explicit claim protocol selection leaves the existing unversioned API intact.
macro_rules! claim_token_entry {
    ($legacy:ident, $tagged:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $legacy<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            token: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                const OP: &str = "mobile.continuation.claim";
                let host = require_host(OP)?;
                let token = read_text(env, &token, OP)?;
                host.$method(&token, continuation_callback(env, &callback, OP)?);
                Ok(())
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.claim")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $tagged<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            token: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                const OP: &str = "mobile.continuation.claim";
                let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
                let token = read_text(env, &token, OP)?;
                host.$method(&token, continuation_callback(env, &callback, OP)?);
                Ok(())
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.claim")
        }
    };
}
claim_token_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaimV1,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaimV1ForBinding,
    continuation_acknowledge_retained_claim_v1
);
// V2 is deliberately a distinct symbol. A 5.2.1 Java bridge has no way to
// invoke it, so it can never acknowledge a descriptor it did not negotiate.
claim_token_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaimV2,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgeClaimV2ForBinding,
    continuation_acknowledge_retained_claim_v2
);
claim_token_entry!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationRetireClaimReceipt,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationRetireClaimReceiptForBinding,
    continuation_retire_claim_receipt
);

fn peer_failure_recovery_read(
    env: &mut Env,
    host: &MobileHost,
    record: &JString,
    offset: &JString,
    callback: &JObject,
) -> MobileResult<()> {
    const OP: &str = "mobile.continuation.peer-source-failure.read";
    let record = read_text(env, record, OP)?;
    let offset_text = read_text(env, offset, OP)?;
    let offset = offset_text
        .parse::<u64>()
        .map_err(|_| invalid(OP, "offset"))?;
    if offset.to_string() != offset_text {
        return Err(invalid(OP, "offset"));
    }
    host.continuation_read_peer_failure_recovery(
        &record,
        offset,
        continuation_callback(env, callback, OP)?,
    );
    Ok(())
}

macro_rules! peer_failure_recovery_entries {
    ($read:ident, $read_bound:ident, $ack:ident, $ack_bound:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $read<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            record: JString<'caller>,
            offset: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = require_host("mobile.continuation.peer-source-failure.read")?;
                peer_failure_recovery_read(env, &host, &record, &offset, &callback)
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.peer-source-failure.read")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $read_bound<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            record: JString<'caller>,
            offset: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = binding_host(binding).ok_or_else(|| {
                    invalid(
                        "mobile.continuation.peer-source-failure.read",
                        "unknown binding",
                    )
                })?;
                peer_failure_recovery_read(env, &host, &record, &offset, &callback)
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.peer-source-failure.read")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $ack<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            record: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                const OP: &str = "mobile.continuation.peer-source-failure.acknowledge";
                let record = read_text(env, &record, OP)?;
                require_host(OP)?.continuation_acknowledge_peer_failure_recovery(
                    &record,
                    continuation_callback(env, &callback, OP)?,
                );
                Ok(())
            })
            .resolve_with::<ThrowMobile, _>(|| {
                "mobile.continuation.peer-source-failure.acknowledge"
            })
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $ack_bound<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            record: JString<'caller>,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                const OP: &str = "mobile.continuation.peer-source-failure.acknowledge";
                let record = read_text(env, &record, OP)?;
                let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
                host.continuation_acknowledge_peer_failure_recovery(
                    &record,
                    continuation_callback(env, &callback, OP)?,
                );
                Ok(())
            })
            .resolve_with::<ThrowMobile, _>(|| {
                "mobile.continuation.peer-source-failure.acknowledge"
            })
        }
    };
}
peer_failure_recovery_entries!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationReadPeerFailureRecovery,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationReadPeerFailureRecoveryForBinding,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgePeerFailureRecovery,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationAcknowledgePeerFailureRecoveryForBinding
);

fn legacy_evidence_page(
    env: &mut Env,
    host: &MobileHost,
    token: &JString,
    after_ordinal: jlong,
    max_items: jint,
    max_bytes: jint,
    callback: &JObject,
) -> MobileResult<()> {
    const OP: &str = "mobile.continuation.legacy-evidence.page";
    let token = read_text(env, token, OP)?;
    let after_ordinal = to_u64(after_ordinal, OP)?;
    let max_items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
    let max_bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
    if max_items == 0 || max_bytes == 0 {
        return Err(invalid(OP, "page bounds must be positive"));
    }
    host.continuation_legacy_evidence_page(
        &token,
        after_ordinal,
        max_items,
        max_bytes,
        continuation_callback(env, callback, OP)?,
    );
    Ok(())
}

fn legacy_evidence_retire(
    env: &mut Env,
    host: &MobileHost,
    token: &JString,
    ordinal: jlong,
    callback: &JObject,
) -> MobileResult<()> {
    const OP: &str = "mobile.continuation.legacy-evidence.retire";
    let token = read_text(env, token, OP)?;
    let ordinal = to_u64(ordinal, OP)?;
    host.continuation_legacy_evidence_retire(
        &token,
        ordinal,
        continuation_callback(env, callback, OP)?,
    );
    Ok(())
}

macro_rules! legacy_evidence_entries {
    ($page:ident, $page_bound:ident, $retire:ident, $retire_bound:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $page<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            token: JString<'caller>,
            after_ordinal: jlong,
            max_items: jint,
            max_bytes: jint,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = require_host("mobile.continuation.legacy-evidence.page")?;
                legacy_evidence_page(
                    env,
                    &host,
                    &token,
                    after_ordinal,
                    max_items,
                    max_bytes,
                    &callback,
                )
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.legacy-evidence.page")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $page_bound<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            token: JString<'caller>,
            after_ordinal: jlong,
            max_items: jint,
            max_bytes: jint,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = binding_host(binding).ok_or_else(|| {
                    invalid(
                        "mobile.continuation.legacy-evidence.page",
                        "unknown binding",
                    )
                })?;
                legacy_evidence_page(
                    env,
                    &host,
                    &token,
                    after_ordinal,
                    max_items,
                    max_bytes,
                    &callback,
                )
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.legacy-evidence.page")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $retire<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            token: JString<'caller>,
            ordinal: jlong,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = require_host("mobile.continuation.legacy-evidence.retire")?;
                legacy_evidence_retire(env, &host, &token, ordinal, &callback)
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.legacy-evidence.retire")
        }
        #[unsafe(no_mangle)]
        pub extern "system" fn $retire_bound<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            binding: jlong,
            token: JString<'caller>,
            ordinal: jlong,
            callback: JObject<'caller>,
        ) {
            env.with_env(|env| -> MobileResult<()> {
                let host = binding_host(binding).ok_or_else(|| {
                    invalid(
                        "mobile.continuation.legacy-evidence.retire",
                        "unknown binding",
                    )
                })?;
                legacy_evidence_retire(env, &host, &token, ordinal, &callback)
            })
            .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.legacy-evidence.retire")
        }
    };
}
legacy_evidence_entries!(
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationLegacyEvidencePage,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationLegacyEvidencePageForBinding,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationLegacyEvidenceRetire,
    Java_com_ubm_core_MobileCoreBridge_nativeContinuationLegacyEvidenceRetireForBinding
);

fn prepare_versioned_claim(
    env: &mut Env,
    host: &MobileHost,
    version: jint,
    items: jint,
    bytes: jint,
    callback: &JObject,
) -> MobileResult<()> {
    const OP: &str = "mobile.continuation.prepare";
    let version = u32::try_from(version).map_err(|_| invalid(OP, "version"))?;
    let items = u32::try_from(items).map_err(|_| invalid(OP, "maxItems"))?;
    let bytes = u32::try_from(bytes).map_err(|_| invalid(OP, "maxBytes"))?;
    if items == 0 || bytes == 0 {
        return Err(invalid(OP, "claim bounds must be positive"));
    }
    host.continuation_prepare_claim_versioned(
        version,
        items,
        bytes,
        continuation_callback(env, callback, OP)?,
    );
    Ok(())
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationPrepareClaimVersioned<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    version: jint,
    items: jint,
    bytes: jint,
    callback: JObject<'caller>,
) {
    env.with_env(|env| -> MobileResult<()> {
        let host = require_host("mobile.continuation.prepare")?;
        prepare_versioned_claim(env, &host, version, items, bytes, &callback)
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.prepare")
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationPrepareClaimVersionedForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    version: jint,
    items: jint,
    bytes: jint,
    callback: JObject<'caller>,
) {
    env.with_env(|env| -> MobileResult<()> {
        let host = binding_host(binding)
            .ok_or_else(|| invalid("mobile.continuation.prepare", "unknown binding"))?;
        prepare_versioned_claim(env, &host, version, items, bytes, &callback)
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.prepare")
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationConfigureRecordingDirectoryForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    path: JString<'caller>,
) -> jstring {
    env.with_env(|env| -> MobileResult<jstring> {
        const OP: &str = "mobile.continuation.recording.configure";
        let _host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
        let path = read_text(env, &path, OP)?;
        let answer = ubm_desktop::continuation::envelope(
            ubm_mobile::continuation::process_recording_registry()
                .configure_directory(std::path::Path::new(&path))
                .map_err(ubm_desktop::continuation::recording_failure),
        );
        Ok(env.new_string(answer)?.into_raw())
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.configure")
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationRecordingControlForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    operation: JString<'caller>,
    id: JString<'caller>,
    token: JString<'caller>,
    items: jint,
    bytes: jint,
) -> jstring {
    env.with_env(|env| -> MobileResult<jstring> {
        const OP: &str = "mobile.continuation.recording";
        let operation = read_text(env, &operation, OP)?;
        let id = read_text(env, &id, OP)?;
        let token = read_text(env, &token, OP)?;
        let items = u32::try_from(items).map_err(|_| invalid(OP, "maxItems"))?;
        let bytes = u32::try_from(bytes).map_err(|_| invalid(OP, "maxBytes"))?;
        // Keep installation admission closed while recorded-route ownership
        // across bindings is not indexed by the journal authority.
        let answer = if operation == "stop" {
            let slot = host_slot()
                .lock()
                .map_err(|_| invalid(OP, "host registry interrupted"))?;
            let host = slot
                .bindings
                .get(&binding)
                .ok_or_else(|| invalid(OP, "unknown binding"))?;
            if recording_stop_permitted(&slot, Some(binding)) {
                host.continuation_recording_control(&operation, &id, &token, items, bytes)
            } else {
                recording_stop_owner_conflict()
            }
        } else {
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            host.continuation_recording_control(&operation, &id, &token, items, bytes)
        };
        Ok(env.new_string(answer)?.into_raw())
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording")
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContinuationOrderedOfflineForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    operation: JString<'caller>,
    id: JString<'caller>,
    callback: JObject<'caller>,
) {
    env.with_env(|env| -> MobileResult<()> {
        const OP: &str = "mobile.continuation.recording.ordered-context-offline";
        let _host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
        let operation = read_text(env, &operation, OP)?;
        let id = read_text(env, &id, OP)?;
        ubm_mobile::continuation::recording_ordered_offline(
            operation,
            id,
            continuation_callback(env, &callback, OP)?,
        );
        Ok(())
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.ordered-context-offline")
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOrderedRecordingVersionForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
) -> jint {
    env.with_env(|_env| -> MobileResult<jint> {
        let _host = binding_host(binding)
            .ok_or_else(|| invalid("mobile.continuation.recording.version", "unknown binding"))?;
        #[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
        if _host.synthetic_marker_enabled_for_test() {
            return Ok(1);
        }
        Ok(0)
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.continuation.recording.version")
}

macro_rules! completion_native {
    ($name:ident, $tagged:ident, $op:literal, ($($param:ident : $ty:ty),*), |$env:ident| $body:expr) => {
        // The immediately-called closure scopes `?` to the value being
        // built, so a malformed Java value fails the request, not the call.
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            request_id: jlong,
            $($param: $ty),*
        ) -> jint {
            unowned_env
                .with_env(|$env| -> MobileResult<jint> {
                    let built: MobileResult<RadioCompletion> = (|| $body)();
                    Ok(complete_or_fail(request_id, built))
                })
                .resolve_with::<ThrowMobile, _>(|| $op)
        }
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $tagged<'caller>(
            mut unowned_env: EnvUnowned<'caller>, _class: JClass<'caller>,
            binding: jlong, request_id: jlong, $($param: $ty),*
        ) -> jint {
            unowned_env.with_env(|$env| -> MobileResult<jint> {
                let Some(host) = binding_host(binding) else { return Ok(STATUS_NO_HOST); };
                let built: MobileResult<RadioCompletion> = (|| $body)();
                Ok(complete_for_binding(&host, request_id, built))
            }).resolve_with::<ThrowMobile, _>(|| $op)
        }
    };
}

const ORDERED_OP: &str = "mobile.complete.ordered";

fn ordered_field<'a>(
    env: &mut Env<'a>,
    obj: &JObject,
    name: &str,
    sig: &str,
) -> MobileResult<jni::JValueOwned<'a>> {
    let signature = jni::signature::RuntimeFieldSignature::from_str(sig)?;
    Ok(env.get_field(
        obj,
        JNIString::from(name),
        jni::signature::FieldSignature::from(&signature),
    )?)
}
fn ordered_text(env: &mut Env, obj: &JObject, name: &str) -> MobileResult<Option<String>> {
    let value = ordered_field(env, obj, name, "Ljava/lang/String;")?.l()?;
    let value = env.cast_local::<JString>(value)?;
    read_opt_text(env, &value, ORDERED_OP)
}
fn ordered_long(env: &mut Env, obj: &JObject, name: &str) -> MobileResult<u64> {
    Ok(epoch_from_jlong(ordered_field(env, obj, name, "J")?.j()?))
}
fn ordered_bool(env: &mut Env, obj: &JObject, name: &str) -> MobileResult<bool> {
    Ok(ordered_field(env, obj, name, "Z")?.z()?)
}
fn ordered_registration(
    env: &mut Env,
    obj: &JObject,
) -> MobileResult<ubm_mobile::OrderedRegistration> {
    if obj.as_raw().is_null() {
        return Err(invalid(ORDERED_OP, "missing registration"));
    }
    Ok(ubm_mobile::OrderedRegistration {
        member_id: ordered_long(env, obj, "member")?,
        source_member: ordered_long(env, obj, "sourceMember")?,
        epoch: ordered_long(env, obj, "epoch")?,
    })
}
fn ordered_member(
    env: &mut Env,
    obj: &JObject,
    name: &str,
) -> MobileResult<Option<ubm_mobile::OrderedRegistration>> {
    let value = ordered_field(
        env,
        obj,
        name,
        "Lcom/ubm/core/MobileCoreBridge$OrderedRegistration;",
    )?
    .l()?;
    if value.as_raw().is_null() {
        Ok(None)
    } else {
        Ok(Some(ordered_registration(env, &value)?))
    }
}
fn ordered_required_member(
    env: &mut Env,
    obj: &JObject,
    name: &str,
) -> MobileResult<ubm_mobile::OrderedRegistration> {
    ordered_member(env, obj, name)?.ok_or_else(|| invalid(ORDERED_OP, "missing member"))
}
fn ordered_required_text(env: &mut Env, obj: &JObject, name: &str) -> MobileResult<String> {
    ordered_text(env, obj, name)?.ok_or_else(|| invalid(ORDERED_OP, "missing text"))
}
fn ordered_generation(
    env: &mut Env,
    obj: &JObject,
) -> MobileResult<ubm_mobile::OrderedSourceGeneration> {
    let kind = ordered_required_text(env, obj, "generationKind")?;
    let value = ordered_required_text(env, obj, "generationValue")?;
    match kind.as_str() {
        "counter" => {
            let counter: u64 = value
                .parse()
                .map_err(|_| invalid(ORDERED_OP, "invalid generation"))?;
            if counter.to_string() != value {
                return Err(invalid(ORDERED_OP, "noncanonical generation"));
            }
            Ok(ubm_mobile::OrderedSourceGeneration::Counter(counter))
        }
        "opaque" => Ok(ubm_mobile::OrderedSourceGeneration::Opaque(value)),
        _ => Err(invalid(ORDERED_OP, "invalid generation kind")),
    }
}
fn ordered_loss(
    env: &mut Env,
    obj: &JObject,
) -> MobileResult<(
    ubm_mobile::OrderedLossScope,
    ubm_mobile::OrderedLossUnit,
    Option<u64>,
    Option<u64>,
)> {
    use ubm_mobile::{OrderedLossScope as S, OrderedLossUnit as U};
    let scope = match ordered_required_text(env, obj, "scope")?.as_str() {
        "member" => S::Member(ordered_long(env, obj, "scopeMember")?),
        "group" => S::Group,
        "unknown" => S::Unknown,
        _ => return Err(invalid(ORDERED_OP, "invalid loss scope")),
    };
    let unit = match ordered_required_text(env, obj, "unit")?.as_str() {
        "values" => U::Values,
        "source-records" => U::SourceRecords,
        _ => return Err(invalid(ORDERED_OP, "invalid loss unit")),
    };
    let count = if ordered_bool(env, obj, "hasCount")? {
        Some(ordered_long(env, obj, "count")?)
    } else {
        None
    };
    let bytes = if ordered_bool(env, obj, "hasByteCount")? {
        Some(ordered_long(env, obj, "byteCount")?)
    } else {
        None
    };
    Ok((scope, unit, count, bytes))
}
fn ordered_record(env: &mut Env, obj: &JObject) -> MobileResult<ubm_mobile::OrderedRecord> {
    use ubm_mobile::{
        OrderedEnd as E, OrderedProvenance as P, OrderedRecord as R, OrderedRejected as X,
    };
    Ok(match ordered_required_text(env, obj, "kind")?.as_str() {
        "value" => {
            let member = ordered_required_member(env, obj, "member")?;
            let ordinal = ordered_long(env, obj, "ordinal")?;
            let value = ordered_field(env, obj, "value", "[B")?.l()?;
            let value = env.cast_local::<JByteArray>(value)?;
            let value = read_bytes(env, &value, ORDERED_OP)?;
            let provenance = match ordered_required_text(env, obj, "provenance")?.as_str() {
                "notification" => P::Notification,
                "read-or-notification" => P::ReadOrNotification,
                "unknown" => P::Unknown,
                _ => return Err(invalid(ORDERED_OP, "invalid provenance")),
            };
            let delivery = match ordered_required_text(env, obj, "delivery")?.as_str() {
                "notification" => ObservedDelivery::Notification,
                "indication" => ObservedDelivery::Indication,
                "unknown" => ObservedDelivery::Unknown,
                _ => return Err(invalid(ORDERED_OP, "invalid delivery")),
            };
            R::Value {
                member,
                ordinal,
                value,
                provenance,
                delivery,
            }
        }
        "loss" => {
            let (scope, unit, count, byte_count) = ordered_loss(env, obj)?;
            R::Loss {
                scope,
                unit,
                count,
                byte_count,
            }
        }
        "fence" => R::Fence {
            member: ordered_required_member(env, obj, "member")?,
            uncertain_tail: ordered_bool(env, obj, "uncertainTail")?,
        },
        "barrier" => R::Barrier {
            barrier_id: ordered_long(env, obj, "barrierId")?,
            uncertain_tail: ordered_bool(env, obj, "uncertainTail")?,
        },
        "marker" => R::Marker {
            marker_id: ordered_long(env, obj, "markerId")?,
            ordinal: ordered_long(env, obj, "ordinal")?,
        },
        "subsequent-source-failure" => R::SubsequentSourceFailure {
            failure: ordered_failure(env, obj)?,
        },
        "failure-observations-omitted" => R::FailureObservationsOmitted,
        "bridge-failure" => R::BridgeFailure {
            code: ordered_required_text(env, obj, "bridgeCode")?,
            detail: ordered_required_text(env, obj, "detail")?,
        },
        "terminal" => {
            let reason = match ordered_required_text(env, obj, "reason")?.as_str() {
                "closed" => E::Closed,
                "connection-lost" => E::ConnectionLost,
                "service-changed" => E::ServiceChanged,
                "source-replaced" => E::SourceReplaced,
                "shutdown" => E::Shutdown,
                "adapter-lost" => E::AdapterLost,
                "overflow" => E::Overflow,
                "ordinal-exhausted" => E::OrdinalExhausted,
                "source-failed" => E::SourceFailed,
                "wake-failed" => E::WakeFailed,
                _ => return Err(invalid(ORDERED_OP, "invalid terminal reason")),
            };
            let rejected = match ordered_text(env, obj, "rejectedKind")?.as_deref() {
                None => None,
                Some("value") => Some(X::Value {
                    member: ordered_required_member(env, obj, "rejectedMember")?,
                    byte_count: ordered_long(env, obj, "rejectedByteCount")?,
                }),
                Some("loss") => {
                    let (scope, unit, count, byte_count) = ordered_loss(env, obj)?;
                    Some(X::Loss {
                        scope,
                        unit,
                        count,
                        byte_count,
                    })
                }
                Some("fence") => Some(X::Fence {
                    member: ordered_required_member(env, obj, "rejectedMember")?,
                }),
                Some("barrier") => Some(X::Barrier {
                    barrier_id: ordered_long(env, obj, "rejectedBarrierId")?,
                }),
                Some("marker") => Some(X::Marker {
                    marker_id: ordered_long(env, obj, "rejectedMarkerId")?,
                }),
                Some("unmapped") => Some(X::Unmapped),
                _ => return Err(invalid(ORDERED_OP, "invalid rejected kind")),
            };
            let native_domain = ordered_text(env, obj, "nativeDomain")?;
            let native_code = ordered_text(env, obj, "nativeCode")?;
            if native_domain.is_some() != native_code.is_some() {
                return Err(invalid(ORDERED_OP, "incomplete native failure"));
            }
            let uncertain_tail = ordered_bool(env, obj, "uncertainTail")?;
            let failure_member = ordered_member(env, obj, "failureMember")?;
            if ordered_text(env, obj, "failureKind")?.is_some() {
                if reason != E::SourceFailed {
                    return Err(invalid(ORDERED_OP, "failure on non-source terminal"));
                }
                R::SourceFailed {
                    uncertain_tail,
                    failure: ordered_failure(env, obj)?,
                    native_domain,
                    native_code,
                    failure_member,
                    rejected,
                }
            } else {
                R::Terminal {
                    reason,
                    uncertain_tail,
                    native_domain,
                    native_code,
                    failure_member,
                    rejected,
                }
            }
        }
        _ => return Err(invalid(ORDERED_OP, "invalid record kind")),
    })
}
fn ordered_members(
    env: &mut Env,
    obj: &JObject,
) -> MobileResult<Vec<ubm_mobile::OrderedRegistration>> {
    let array = ordered_field(
        env,
        obj,
        "members",
        "[Lcom/ubm/core/MobileCoreBridge$OrderedRegistration;",
    )?
    .l()?;
    if array.as_raw().is_null() {
        return Err(invalid(ORDERED_OP, "missing members"));
    }
    let array = env.cast_local::<JObjectArray<JObject>>(array)?;
    let mut members = Vec::new();
    for index in 0..array.len(env)? {
        members.push(env.with_local_frame(32, |env| {
            let item = array.get_element(env, index)?;
            ordered_registration(env, &item)
        })?);
    }
    Ok(members)
}
fn ordered_failure(env: &mut Env, obj: &JObject) -> MobileResult<PlatformFailure> {
    let kind = ordered_required_text(env, obj, "failureKind")?;
    Ok(PlatformFailure {
        kind: parse_enum(&kind, FailureKind::parse, ORDERED_OP)?,
        gatt_status: optional_int(ordered_field(env, obj, "gattStatus", "I")?.i()?),
        native_domain: None,
        native_code: None,
        native_name: ordered_text(env, obj, "nativeName")?.filter(|code| !code.is_empty()),
        detail: ordered_text(env, obj, "detail")?.unwrap_or_default(),
        dispatched: ordered_bool(env, obj, "dispatched")?,
    })
}

fn ordered_failure_v2(env: &mut Env, obj: &JObject) -> MobileResult<PlatformFailure> {
    let mut failure = ordered_failure(env, obj)?;
    failure.native_domain = ordered_text(env, obj, "nativeDomain")?;
    failure.native_code = if ordered_bool(env, obj, "hasNativeCode")? {
        Some(ordered_field(env, obj, "nativeCode", "J")?.j()?)
    } else {
        None
    };
    Ok(failure)
}

fn owner_retirement_completion(env: &mut Env, obj: &JObject) -> MobileResult<RadioCompletion> {
    use ubm_mobile::{
        OrderedAdmissionOwner, OwnerOutstandingClass as C, OwnerRetirementRejection as R,
        OwnerRetirementResult, OwnerRetirementState as S,
    };
    if obj.as_raw().is_null() {
        return Err(invalid(ORDERED_OP, "missing owner retirement result"));
    }
    let parse_id = |s: String| {
        OrderedAdmissionOwner::negotiate_slot(2, &s)
            .map_err(|_| invalid(ORDERED_OP, "invalid owner retirement identity"))
    };
    let owner_incarnation = parse_id(ordered_required_text(env, obj, "ownerIncarnation")?)?;
    let retirement_id = parse_id(ordered_required_text(env, obj, "retirementId")?)?;
    let state = match ordered_required_text(env, obj, "state")?.as_str() {
        "quiescent" => {
            let classes =
                ordered_field(env, obj, "outstandingClasses", "[Ljava/lang/String;")?.l()?;
            let failures = ordered_field(
                env,
                obj,
                "failures",
                "[Lcom/ubm/core/MobileCoreBridge$OrderedResult;",
            )?
            .l()?;
            if classes.as_raw().is_null() || failures.as_raw().is_null() {
                return Err(invalid(ORDERED_OP, "missing quiescence details"));
            }
            let classes = env.cast_local::<JObjectArray<JString>>(classes)?;
            let failures = env.cast_local::<JObjectArray<JObject>>(failures)?;
            if classes.len(env)? != 0
                || failures.len(env)? != 0
                || ordered_text(env, obj, "reason")?.is_some_and(|reason| !reason.is_empty())
            {
                return Err(invalid(ORDERED_OP, "contradictory quiescence details"));
            }
            S::Quiescent
        }
        "rejected" => S::Rejected {
            reason: match ordered_required_text(env, obj, "reason")?.as_str() {
                "wrong-owner" => R::WrongOwner,
                "conflicting-retirement" => R::ConflictingRetirement,
                "malformed" => R::Malformed,
                _ => return Err(invalid(ORDERED_OP, "invalid owner rejection")),
            },
        },
        "fenced-pending" => {
            if ordered_text(env, obj, "reason")?.is_some_and(|reason| !reason.is_empty()) {
                return Err(invalid(ORDERED_OP, "contradictory pending reason"));
            }
            let classes =
                ordered_field(env, obj, "outstandingClasses", "[Ljava/lang/String;")?.l()?;
            let failures = ordered_field(
                env,
                obj,
                "failures",
                "[Lcom/ubm/core/MobileCoreBridge$OrderedResult;",
            )?
            .l()?;
            if classes.as_raw().is_null() || failures.as_raw().is_null() {
                return Err(invalid(ORDERED_OP, "missing owner retirement details"));
            }
            let classes = env.cast_local::<JObjectArray<JString>>(classes)?;
            let failures = env.cast_local::<JObjectArray<JObject>>(failures)?;
            let mut outstanding_classes = Vec::new();
            for i in 0..classes.len(env)? {
                let value = classes.get_element(env, i)?;
                outstanding_classes.push(match read_text(env, &value, ORDERED_OP)?.as_str() {
                    "attachments" => C::Attachments,
                    "reservations" => C::Reservations,
                    "native-work" => C::NativeWork,
                    "callbacks" => C::Callbacks,
                    "cleanup" => C::Cleanup,
                    _ => return Err(invalid(ORDERED_OP, "invalid outstanding class")),
                });
            }
            let mut decoded_failures = Vec::new();
            for i in 0..failures.len(env)? {
                let value = failures.get_element(env, i)?;
                if value.as_raw().is_null() {
                    return Err(invalid(ORDERED_OP, "missing retirement failure"));
                }
                decoded_failures.push(ordered_failure_v2(env, &value)?);
            }
            S::FencedPending {
                outstanding_classes,
                failures: decoded_failures,
            }
        }
        _ => return Err(invalid(ORDERED_OP, "invalid owner retirement state")),
    };
    Ok(RadioCompletion::OwnerRetirement(OwnerRetirementResult {
        owner_incarnation,
        retirement_id,
        state,
    }))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteOwnerRetirement<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request_id: jlong,
    result: JObject<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> MobileResult<jint> {
            Ok(complete_or_fail(
                request_id,
                owner_retirement_completion(env, &result),
            ))
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.complete.owner-retirement")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteOwnerRetirementForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    request_id: jlong,
    result: JObject<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> MobileResult<jint> {
            if let Some(host) = binding_host(binding) {
                return Ok(complete_for_binding(
                    &host,
                    request_id,
                    owner_retirement_completion(env, &result),
                ));
            }
            let pending = {
                let slot = host_slot()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(entry) = slot.unpublished.get(&binding) else {
                    return Ok(STATUS_NO_HOST);
                };
                entry
                    .pending
                    .as_ref()
                    .filter(|(id, _)| *id == request_id)
                    .map(|(_, tx)| tx.clone())
            };
            let Some(pending) = pending else {
                return Ok(completion_status(CompletionStatus::Late));
            };
            let completion = owner_retirement_completion(env, &result).unwrap_or_else(|error| {
                RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    format!("malformed owner retirement: {error:?}"),
                ))
            });
            Ok(if pending.try_send(completion).is_ok() {
                0
            } else {
                completion_status(CompletionStatus::Late)
            })
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.complete.owner-retirement")
}

fn ordered_completion(env: &mut Env, obj: &JObject) -> MobileResult<RadioCompletion> {
    if obj.as_raw().is_null() {
        return Err(invalid(ORDERED_OP, "missing result"));
    }
    Ok(match ordered_required_text(env, obj, "kind")?.as_str() {
        "open-result" | "open-retired" => {
            use ubm_mobile::{
                OrderedAdmissionOwner, OrderedOpenCompletionV2 as C, OrderedOpenOutcomeV2 as O,
                OrderedOpenOwnershipV2 as Ownership, OrderedOpenResultV2, OrderedOpenRetired,
                OrderedSlotIdentity,
            };
            let owner_text = ordered_required_text(env, obj, "ownerIncarnation")?;
            let owner_incarnation = OrderedAdmissionOwner::negotiate_slot(2, &owner_text)
                .map_err(|_| invalid(ORDERED_OP, "invalid slot owner"))?;
            let slot = u32::try_from(ordered_field(env, obj, "slot", "I")?.i()?)
                .map_err(|_| invalid(ORDERED_OP, "invalid slot"))?;
            let slot_generation = ordered_long(env, obj, "slotGeneration")?;
            if slot >= 64 || slot_generation == 0 {
                return Err(invalid(ORDERED_OP, "invalid slot identity"));
            }
            let slot_identity = OrderedSlotIdentity {
                owner_incarnation,
                slot,
                slot_generation,
            };
            if ordered_required_text(env, obj, "kind")? == "open-retired" {
                RadioCompletion::OrderedOpenV2(C::Retired(OrderedOpenRetired { slot_identity }))
            } else {
                let outcome = match ordered_required_text(env, obj, "status")?.as_str() {
                    "pending" => O::Pending,
                    "opened" => O::Opened {
                        source_generation: ordered_generation(env, obj)?,
                        members: ordered_members(env, obj)?,
                    },
                    "failed" => {
                        let original_failure = ordered_failure_v2(env, obj)?;
                        let ownership =
                            match ordered_required_text(env, obj, "ownershipKind")?.as_str() {
                                "not-created" => Ownership::NotCreated,
                                "created" => Ownership::Created {
                                    source_generation: ordered_generation(env, obj)?,
                                    members: ordered_members(env, obj)?,
                                },
                                _ => return Err(invalid(ORDERED_OP, "invalid v2 open ownership")),
                            };
                        O::Failed {
                            original_failure,
                            ownership,
                        }
                    }
                    "indeterminate" => O::Indeterminate {
                        original_failure_or_reason: match ordered_required_text(
                            env,
                            obj,
                            "reasonKind",
                        )?
                        .as_str()
                        {
                            "failure" => ubm_mobile::OrderedOpenIndeterminateReason::Failure(
                                ordered_failure_v2(env, obj)?,
                            ),
                            "reason" => ubm_mobile::OrderedOpenIndeterminateReason::Reason(
                                ordered_required_text(env, obj, "detail")?,
                            ),
                            _ => return Err(invalid(ORDERED_OP, "invalid indeterminate reason")),
                        },
                    },
                    _ => return Err(invalid(ORDERED_OP, "invalid v2 open status")),
                };
                RadioCompletion::OrderedOpenV2(C::Result(OrderedOpenResultV2 {
                    slot_identity,
                    outcome,
                }))
            }
        }
        "opened" => RadioCompletion::OrderedOpened {
            generation: ordered_generation(env, obj)?,
            members: ordered_members(env, obj)?,
        },
        "open-failed" => {
            use ubm_mobile::OrderedOpenOwnership as O;
            let failure = ordered_failure(env, obj)?;
            let ownership = match ordered_required_text(env, obj, "ownershipKind")?.as_str() {
                "not-created" => O::NotCreated,
                "created" => O::Created {
                    generation: ordered_generation(env, obj)?,
                    members: ordered_members(env, obj)?,
                },
                "indeterminate" => O::Indeterminate {
                    recovery_key: ordered_long(env, obj, "recoveryKey")?,
                },
                _ => return Err(invalid(ORDERED_OP, "invalid open ownership")),
            };
            RadioCompletion::OrderedOpenFailed { failure, ownership }
        }
        "graph-reserve-status" => RadioCompletion::OrderedGraphReserveStatus(
            match ordered_required_text(env, obj, "status")?.as_str() {
                "pending" => ubm_mobile::OrderedGraphReserveStatus::Pending,
                "not-created" => ubm_mobile::OrderedGraphReserveStatus::NotCreated,
                "issued" => ubm_mobile::OrderedGraphReserveStatus::Issued(ordered_long(
                    env,
                    obj,
                    "authorityId",
                )?),
                _ => return Err(invalid(ORDERED_OP, "invalid reservation status")),
            },
        ),
        "graph-permit-reserved" => {
            RadioCompletion::OrderedGraphPermitReserved(ordered_long(env, obj, "authorityId")?)
        }
        "graph-authority-reserved" => {
            RadioCompletion::OrderedGraphAuthorityReserved(ordered_long(env, obj, "authorityId")?)
        }
        "graph-authority-status" => RadioCompletion::OrderedGraphAuthorityStatus(
            match ordered_required_text(env, obj, "status")?.as_str() {
                "pending" => ubm_mobile::OrderedGraphAuthorityStatus::Pending,
                "not-created" => ubm_mobile::OrderedGraphAuthorityStatus::NotCreated,
                "opened" => ubm_mobile::OrderedGraphAuthorityStatus::Opened(ordered_long(
                    env,
                    obj,
                    "authorityId",
                )?),
                "closed" => ubm_mobile::OrderedGraphAuthorityStatus::Closed(ordered_long(
                    env,
                    obj,
                    "authorityId",
                )?),
                _ => return Err(invalid(ORDERED_OP, "invalid authority status")),
            },
        ),
        "graph-authority-opened" => {
            RadioCompletion::OrderedGraphAuthorityOpened(ordered_long(env, obj, "authorityId")?)
        }
        "graph-permit-not-acquired" => {
            RadioCompletion::OrderedGraphPermitNotAcquired(ordered_failure(env, obj)?)
        }
        "graph-permit-invalidated" => {
            RadioCompletion::OrderedGraphPermitInvalidated(ordered_failure(env, obj)?)
        }
        "graph-permit" => RadioCompletion::OrderedGraphPermit(
            match ordered_required_text(env, obj, "status")?.as_str() {
                "acquired" => ubm_mobile::OrderedGraphPermitState::Acquired,
                "stale" => ubm_mobile::OrderedGraphPermitState::Stale,
                "busy" => ubm_mobile::OrderedGraphPermitState::Busy,
                "released" => ubm_mobile::OrderedGraphPermitState::Released,
                "committed" => ubm_mobile::OrderedGraphPermitState::Committed,
                "forgotten" => ubm_mobile::OrderedGraphPermitState::Forgotten,
                _ => return Err(invalid(ORDERED_OP, "invalid graph permit status")),
            },
        ),
        "marker-write-status" => {
            use ubm_mobile::{
                OrderedMarkerAdmission as A, OrderedMarkerWriteState as W,
                OrderedMarkerWriteStatus as S,
            };
            let admission = match ordered_required_text(env, obj, "admission")?.as_str() {
                "pending" => A::Pending,
                "not-admitted" => A::NotAdmitted,
                "admitted" => A::Admitted {
                    ordinal: ordered_long(env, obj, "ordinal")?,
                },
                "rejected" => A::Rejected {
                    failure: ordered_failure(env, obj)?,
                },
                _ => return Err(invalid(ORDERED_OP, "invalid marker admission")),
            };
            let write = match ordered_required_text(env, obj, "writeState")?.as_str() {
                "initiating" => W::Initiating,
                "not-started" => W::NotStarted {
                    failure: if ordered_bool(env, obj, "hasFailure")? {
                        Some(ordered_failure(env, obj)?)
                    } else {
                        None
                    },
                },
                "started" => W::Started {
                    failure: if ordered_bool(env, obj, "hasFailure")? {
                        Some(ordered_failure(env, obj)?)
                    } else {
                        None
                    },
                },
                "settled" => W::Settled {
                    failure: if ordered_bool(env, obj, "hasFailure")? {
                        Some(ordered_failure(env, obj)?)
                    } else {
                        None
                    },
                },
                _ => return Err(invalid(ORDERED_OP, "invalid marker write state")),
            };
            RadioCompletion::OrderedMarkerWriteStatus(S { admission, write })
        }
        "enable-status" => {
            use ubm_mobile::OrderedEnableStatus as S;
            let status = match ordered_required_text(env, obj, "status")?.as_str() {
                "pending" => S::Pending,
                "enabled" => S::Enabled(
                    match ordered_required_text(env, obj, "delivery")?.as_str() {
                        "notification" => ObservedDelivery::Notification,
                        "indication" => ObservedDelivery::Indication,
                        "unknown" => ObservedDelivery::Unknown,
                        _ => return Err(invalid(ORDERED_OP, "invalid delivery")),
                    },
                ),
                "released" => S::Released(if ordered_bool(env, obj, "hasFailure")? {
                    Some(ordered_failure(env, obj)?)
                } else {
                    None
                }),
                "uncertain" => S::Uncertain(ordered_failure(env, obj)?),
                _ => return Err(invalid(ORDERED_OP, "invalid enable status")),
            };
            RadioCompletion::OrderedEnableStatus(status)
        }
        "registered" => {
            RadioCompletion::OrderedRegistered(ordered_required_member(env, obj, "member")?)
        }
        "claim" => RadioCompletion::OrderedClaim {
            generation: ordered_generation(env, obj)?,
            member: ordered_required_member(env, obj, "member")?,
        },
        "records" => {
            let batch_id = if ordered_bool(env, obj, "hasBatch")? {
                Some(ordered_long(env, obj, "batch")?)
            } else {
                None
            };
            let array = ordered_field(
                env,
                obj,
                "records",
                "[Lcom/ubm/core/MobileCoreBridge$OrderedRecord;",
            )?
            .l()?;
            if array.as_raw().is_null() {
                return Err(invalid(ORDERED_OP, "missing records"));
            }
            let array = env.cast_local::<JObjectArray<JObject>>(array)?;
            let mut records = Vec::new();
            for index in 0..array.len(env)? {
                records.push(env.with_local_frame(64, |env| {
                    let item = array.get_element(env, index)?;
                    ordered_record(env, &item)
                })?);
            }
            if batch_id.is_some() == records.is_empty() {
                return Err(invalid(ORDERED_OP, "invalid batch"));
            }
            RadioCompletion::OrderedRecords { batch_id, records }
        }
        _ => return Err(invalid(ORDERED_OP, "invalid result kind")),
    })
}
completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteOrdered,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteOrderedForBinding,
    "mobile.complete.ordered",
    (result: JObject<'caller>),
    |env| ordered_completion(env, &result)
);

/// Scan tokens are the exact positive request id given to the radio before
/// startScan is entered. They are available even to synchronous callbacks.
fn scan_identity_id(env: &mut Env, value: &JString, operation: &'static str) -> MobileResult<u64> {
    let value = read_text(env, value, operation)?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| invalid(operation, "invalid scan identity"))?;
    if !valid_request_id(parsed) || parsed.to_string() != value {
        return Err(invalid(operation, "invalid scan identity"));
    }
    Ok(parsed)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanIdentityWireVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    2
}

fn scan_started(
    env: &mut Env,
    request_id: jlong,
    identity: &JString,
) -> MobileResult<RadioCompletion> {
    let id = scan_identity_id(env, identity, "mobile.complete.scan-started")?;
    if u64::try_from(request_id).ok() != Some(id) {
        return Err(invalid(
            "mobile.complete.scan-started",
            "scan identity differs from request",
        ));
    }
    Ok(RadioCompletion::Unit)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteScanStartedV2<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request_id: jlong,
    identity: JString<'caller>,
) -> jint {
    env.with_env(|env| -> MobileResult<jint> {
        Ok(complete_or_fail(
            request_id,
            scan_started(env, request_id, &identity),
        ))
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.complete.scan-started")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteScanStartedV2ForBinding<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    request_id: jlong,
    identity: JString<'caller>,
) -> jint {
    env.with_env(|env| -> MobileResult<jint> {
        let Some(host) = binding_host(binding) else {
            return Ok(STATUS_NO_HOST);
        };
        Ok(complete_for_binding(
            &host,
            request_id,
            scan_started(env, request_id, &identity),
        ))
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.complete.scan-started")
}

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteUnit,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteUnitForBinding,
    "mobile.complete.unit",
    (),
    |_env| Ok(RadioCompletion::Unit)
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBytes,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBytesForBinding,
    "mobile.complete.bytes",
    (value: JByteArray<'caller>),
    |env| Ok(RadioCompletion::Bytes(read_bytes(env, &value, "mobile.complete.bytes")?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteRead,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteReadForBinding,
    "mobile.complete.read",
    (value: JByteArray<'caller>, provenance: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.read";
        let provenance = ReadProvenance::from_wire(read_text(env, &provenance, OP)?.as_str())
            .ok_or_else(|| invalid(OP, "provenance must be read-response or read-or-notification"))?;
        Ok(RadioCompletion::Read {
            value: read_bytes(env, &value, OP)?,
            provenance,
        })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAdapter,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAdapterForBinding,
    "mobile.complete.adapter",
    (availability: JString<'caller>, authorization: JString<'caller>, power: JString<'caller>, safe_reason: JString<'caller>),
    |env| Ok(RadioCompletion::Adapter(adapter_snapshot(
        env,
        &availability,
        &authorization,
        &power,
        &safe_reason,
        "mobile.complete.adapter"
    )?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteDiscovered,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteDiscoveredForBinding,
    "mobile.complete.discovered",
    (levels: JIntArray<'caller>, uuids: JObjectArray<'caller, JString<'caller>>, occurrences: JLongArray<'caller>, properties: JIntArray<'caller>),
    |env| {
        const OP: &str = "mobile.complete.discovered";
        let levels = read_ints(env, &levels)?;
        let uuids = read_required_texts(env, &uuids, OP)?;
        let occurrences = read_longs(env, &occurrences)?;
        let properties = read_ints(env, &properties)?;
        discovery_tree(&levels, &uuids, &occurrences, &properties)
            .map(RadioCompletion::Discovered)
            .map_err(|detail| invalid(OP, detail))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteDiscoveredScoped,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteDiscoveredScopedForBinding,
    "mobile.complete.discovered-scoped",
    (levels: JIntArray<'caller>, uuids: JObjectArray<'caller, JString<'caller>>, occurrences: JLongArray<'caller>, properties: JIntArray<'caller>, owner: JString<'caller>, attachment: jlong, revision: jlong),
    |env| {
        const OP: &str = "mobile.complete.discovered-scoped";
        let services = discovery_tree(&read_ints(env, &levels)?, &read_required_texts(env, &uuids, OP)?, &read_longs(env, &occurrences)?, &read_ints(env, &properties)?).map_err(|detail| invalid(OP, detail))?;
        let identity = ubm_desktop::boundary::GattSnapshotIdentity { owner: read_text(env, &owner, OP)?, attachment: epoch_from_jlong(attachment), revision: epoch_from_jlong(revision) };
        Ok(RadioCompletion::DiscoveredScoped { services, identity })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteNotifyEnabled,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteNotifyEnabledForBinding,
    "mobile.complete.notify-enabled",
    (delivery: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.notify-enabled";
        let delivery = match read_text(env, &delivery, OP)?.as_str() {
            "notification" => ObservedDelivery::Notification,
            "indication" => ObservedDelivery::Indication,
            "unknown" => ObservedDelivery::Unknown,
            _ => return Err(invalid(OP, "delivery must be notification, indication or unknown")),
        };
        Ok(RadioCompletion::NotifyEnabled(delivery))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteMtu,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteMtuForBinding,
    "mobile.complete.mtu",
    (mtu: jint),
    |_env| {
        const OP: &str = "mobile.complete.mtu";
        if mtu == 0 {
            return Ok(RadioCompletion::Mtu(None));
        }
        let mtu = u16::try_from(mtu).ok().filter(|mtu| (23..=517).contains(mtu));
        mtu.map(|mtu| RadioCompletion::Mtu(Some(mtu)))
            .ok_or_else(|| invalid(OP, "mtu must be 0 or 23..=517"))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteWriteLimits,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteWriteLimitsForBinding,
    "mobile.complete.write-limits",
    (with_response: jint, without_response: jint),
    |_env| {
        const OP: &str = "mobile.complete.write-limits";
        let limit = |value: jint| u16::try_from(value).ok().filter(|limit| *limit > 0);
        match (limit(with_response), limit(without_response)) {
            (Some(with_response), Some(without_response)) => {
                Ok(RadioCompletion::WriteLimits(ubm_mobile::WriteLimits {
                    with_response,
                    without_response,
                }))
            }
            _ => Err(invalid(OP, "write limits must be 1..=65535 bytes")),
        }
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteRssi,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteRssiForBinding,
    "mobile.complete.rssi",
    (rssi: jint),
    |_env| i8::try_from(rssi)
        .map(|rssi| RadioCompletion::Rssi(i16::from(rssi)))
        .map_err(|_| invalid("mobile.complete.rssi", "rssi out of range"))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAccepted,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAcceptedForBinding,
    "mobile.complete.accepted",
    (accepted: jboolean),
    |_env| Ok(RadioCompletion::Accepted(accepted))
);

fn phy(
    env: &mut Env,
    value: &JString,
    operation: &'static str,
) -> MobileResult<Option<ubm_mobile::Phy>> {
    read_opt_text(env, value, operation)?
        .map(|text| match text.as_str() {
            "le-1m" => Ok(ubm_mobile::Phy::Le1m),
            "le-2m" => Ok(ubm_mobile::Phy::Le2m),
            "le-coded" => Ok(ubm_mobile::Phy::LeCoded),
            _ => Err(invalid(operation, "phy must be le-1m, le-2m or le-coded")),
        })
        .transpose()
}

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhy,
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhyForBinding,
    "mobile.complete.phy",
    (tx: JString<'caller>, rx: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.phy";
        match (phy(env, &tx, OP)?, phy(env, &rx, OP)?) {
            (Some(tx), Some(rx)) => Ok(RadioCompletion::Phy(PhyObservation { tx, rx })),
            _ => Err(invalid(OP, "tx and rx are required")),
        }
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhyRequest,
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhyRequestForBinding,
    "mobile.complete.phy-request",
    (accepted: jboolean, tx: JString<'caller>, rx: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.phy-request";
        let observation = match (phy(env, &tx, OP)?, phy(env, &rx, OP)?) {
            (Some(tx), Some(rx)) => Some(PhyObservation { tx, rx }),
            (None, None) => None,
            _ => return Err(invalid(OP, "tx and rx are both set or both null")),
        };
        Ok(RadioCompletion::PhyRequest { accepted, observation })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteSecurity,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteSecurityForBinding,
    "mobile.complete.security",
    (bond: JString<'caller>, encryption: JString<'caller>, authentication: JString<'caller>, secure_connections: JString<'caller>, pairing_possible: jint),
    |env| Ok(RadioCompletion::Security(security_state(
        env,
        &bond,
        &encryption,
        &authentication,
        &secure_connections,
        pairing_possible,
        "mobile.complete.security"
    )?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBondedPeers,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBondedPeersForBinding,
    "mobile.complete.bonded-peers",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, names: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.bonded-peers";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let names = read_texts(env, &names, OP)?;
        same_length(OP, &[peer_ids.len(), names.len()])?;
        Ok(RadioCompletion::BondedPeers(
            peer_ids
                .into_iter()
                .zip(names)
                .map(|(peer_id, name)| BondedPeer { peer_id, name })
                .collect(),
        ))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteResolvedPeer,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteResolvedPeerForBinding,
    "mobile.complete.resolve-peer",
    (peer_id: JString<'caller>, name: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.resolve-peer";
        let peer_id = read_opt_text(env, &peer_id, OP)?;
        let name = read_opt_text(env, &name, OP)?;
        if peer_id.is_none() && name.is_some() { return Err(invalid(OP, "absent peer with a name")); }
        Ok(RadioCompletion::ResolvedPeer(peer_id.map(|peer_id| ResolvedPeer { peer_id, name })))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteConnectedPeers,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteConnectedPeersForBinding,
    "mobile.complete.connected-peers",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, names: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.connected-peers";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let names = read_texts(env, &names, OP)?;
        same_length(OP, &[peer_ids.len(), names.len()])?;
        Ok(RadioCompletion::ConnectedPeers(
            peer_ids
                .into_iter()
                .zip(names)
                .map(|(peer_id, name)| ConnectedPeer { peer_id, name })
                .collect(),
        ))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteLease,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteLeaseForBinding,
    "mobile.complete.lease",
    (lease_id: JString<'caller>),
    |env| Ok(RadioCompletion::Lease(read_text(env, &lease_id, "mobile.complete.lease")?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteCompanion,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteCompanionForBinding,
    "mobile.complete.companion",
    (association_id: jlong, peer_id: JString<'caller>, display_name: JString<'caller>, already_associated: jboolean),
    |env| {
        const OP: &str = "mobile.complete.companion";
        Ok(RadioCompletion::Companion {
            association_id,
            peer_id: read_opt_text(env, &peer_id, OP)?,
            display_name: read_opt_text(env, &display_name, OP)?,
            already_associated,
        })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteCompanionList,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteCompanionListForBinding,
    "mobile.complete.companion-list",
    (association_ids: JLongArray<'caller>, peer_ids: JObjectArray<'caller, JString<'caller>>, display_names: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.companion-list";
        let association_ids = read_longs(env, &association_ids)?;
        let peer_ids = read_texts(env, &peer_ids, OP)?;
        let display_names = read_texts(env, &display_names, OP)?;
        same_length(
            OP,
            &[
                association_ids.len(),
                peer_ids.len(),
                display_names.len(),
            ],
        )?;
        Ok(RadioCompletion::CompanionList(
            association_ids
                .into_iter()
                .zip(peer_ids)
                .zip(display_names)
                .map(|((association_id, peer_id), display_name)| CompanionRecord {
                    association_id,
                    peer_id,
                    display_name,
                })
                .collect(),
        ))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteClosed,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteClosedForBinding,
    "mobile.complete.closed",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, service_uuids: JObjectArray<'caller, JString<'caller>>, service_occurrences: JLongArray<'caller>, characteristic_uuids: JObjectArray<'caller, JString<'caller>>, characteristic_occurrences: JLongArray<'caller>, details: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.closed";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let service_uuids = read_required_texts(env, &service_uuids, OP)?;
        let service_occurrences = read_longs(env, &service_occurrences)?;
        let characteristic_uuids = read_required_texts(env, &characteristic_uuids, OP)?;
        let characteristic_occurrences = read_longs(env, &characteristic_occurrences)?;
        let details = read_required_texts(env, &details, OP)?;
        same_length(
            OP,
            &[
                peer_ids.len(),
                service_uuids.len(),
                service_occurrences.len(),
                characteristic_uuids.len(),
                characteristic_occurrences.len(),
                details.len(),
            ],
        )?;
        let mut failures = Vec::with_capacity(peer_ids.len());
        for index in 0..peer_ids.len() {
            failures.push(CloseFailure {
                instance: Instance {
                    peer_id: peer_ids[index].clone(),
                    service_uuid: service_uuids[index].clone(),
                    service_occurrence: to_u64(service_occurrences[index], OP)?,
                    characteristic_uuid: characteristic_uuids[index].clone(),
                    characteristic_occurrence: to_u64(characteristic_occurrences[index], OP)?,
                },
                detail: details[index].clone(),
            });
        }
        Ok(RadioCompletion::Closed(failures))
    }
);

fn failure_completion(
    env: &mut Env,
    kind: &JString,
    gatt_status: jint,
    detail: &JString,
    dispatched: jboolean,
    native_code: &JString,
) -> MobileResult<RadioCompletion> {
    const OP: &str = "mobile.complete.failure";
    let kind = read_text(env, kind, OP)?;
    let native_name = read_opt_text(env, native_code, OP)?.filter(|code| !code.is_empty());
    Ok(RadioCompletion::Failed(PlatformFailure {
        kind: parse_enum(&kind, FailureKind::parse, OP)?,
        gatt_status: optional_int(gatt_status),
        native_domain: None,
        native_code: None,
        native_name,
        detail: read_opt_text(env, detail, OP)?.unwrap_or_default(),
        dispatched,
    }))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteFailure<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request_id: jlong,
    kind: JString<'caller>,
    gatt_status: jint,
    detail: JString<'caller>,
    dispatched: jboolean,
    native_code: JString<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> MobileResult<jint> {
            Ok(complete_or_fail(
                request_id,
                failure_completion(env, &kind, gatt_status, &detail, dispatched, &native_code),
            ))
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.complete.failure")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeCompleteFailureForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    request_id: jlong,
    kind: JString<'caller>,
    gatt_status: jint,
    detail: JString<'caller>,
    dispatched: jboolean,
    native_code: JString<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> MobileResult<jint> {
            if let Some(host) = binding_host(binding) {
                return Ok(complete_for_binding(
                    &host,
                    request_id,
                    failure_completion(env, &kind, gatt_status, &detail, dispatched, &native_code),
                ));
            }
            let pending = {
                let slot = host_slot()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(entry) = slot.unpublished.get(&binding) else {
                    return Ok(STATUS_NO_HOST);
                };
                entry
                    .pending
                    .as_ref()
                    .filter(|(id, _)| *id == request_id)
                    .map(|(_, tx)| tx.clone())
            };
            let Some(pending) = pending else {
                return Ok(completion_status(CompletionStatus::Late));
            };
            let completion =
                failure_completion(env, &kind, gatt_status, &detail, dispatched, &native_code)
                    .unwrap_or_else(|error| {
                        RadioCompletion::Failed(PlatformFailure::new(
                            FailureKind::Platform,
                            format!("malformed cleanup failure: {error:?}"),
                        ))
                    });
            Ok(if pending.try_send(completion).is_ok() {
                0
            } else {
                completion_status(CompletionStatus::Late)
            })
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.complete.failure")
}

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteOrderedEnableReleased,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteOrderedEnableReleasedForBinding,
    "mobile.complete.ordered-enable-released",
    (kind: JString<'caller>, gatt_status: jint, detail: JString<'caller>, dispatched: jboolean, native_code: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.ordered-enable-released";
        let kind = read_text(env, &kind, OP)?;
        Ok(RadioCompletion::OrderedEnableReleased(PlatformFailure { kind: parse_enum(&kind, FailureKind::parse, OP)?, gatt_status: optional_int(gatt_status), native_domain: None, native_code: None, native_name: read_opt_text(env, &native_code, OP)?.filter(|code| !code.is_empty()), detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(), dispatched }))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteSubrateStatus,
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteSubrateStatusForBinding,
    "mobile.complete.subrate-status",
    (status: jint),
    |_env| Ok(ubm_mobile::subrate_status_completion(status))
);

// -- natives: ingress ------------------------------------------------------

macro_rules! ingress_native {
    ($name:ident, $tagged:ident, $op:literal, ($($param:ident : $ty:ty),*), |$env:ident| $body:expr) => {
        // See `completion_native!`: the closure scopes `?` to the fact.
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            $($param: $ty),*
        ) -> jint {
            unowned_env
                .with_env(|$env| -> MobileResult<jint> {
                    let built: MobileResult<RadioIngress> = (|| $body)();
                    Ok(match built {
                        Ok(ingress) => ingest(ingress),
                        // An unreadable fact is counted as a drop of its
                        // class and reported, never silently discarded.
                        Err(error) => {
                            eprintln!("ubm-mobile jni [{}]: {error:?}", $op);
                            ingest(RadioIngress::Dropped {
                                class: ingress_class_of($op),
                                detail: format!("unreadable {}", $op),
                            })
                        }
                    })
                })
                .resolve_with::<ThrowMobile, _>(|| $op)
        }
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $tagged<'caller>(
            mut unowned_env: EnvUnowned<'caller>, _class: JClass<'caller>,
            binding: jlong, $($param: $ty),*
        ) -> jint {
            unowned_env.with_env(|$env| -> MobileResult<jint> {
                let Some(host) = binding_host(binding) else { return Ok(STATUS_NO_HOST); };
                let built: MobileResult<RadioIngress> = (|| $body)();
                let ingress = built.unwrap_or_else(|error| {
                    eprintln!("ubm-mobile jni [{}]: {error:?}", $op);
                    RadioIngress::Dropped { class: ingress_class_of($op), detail: format!("unreadable {}", $op) }
                });
                Ok(ingress_status(host.ingest(ingress)))
            }).resolve_with::<ThrowMobile, _>(|| $op)
        }
    };
}

fn ingress_class_of(operation: &str) -> IngressClass {
    match operation {
        "mobile.ingest.advertisement" => IngressClass::Advertisement,
        "mobile.ingest.notification" => IngressClass::Notification,
        _ => IngressClass::Control,
    }
}

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestOrderedWake,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestOrderedWakeForBinding,
    "mobile.ingress.ordered-wake",
    (source_generation: jlong, group_id: jlong),
    |_env| Ok(RadioIngress::OrderedWake {
        generation: ubm_mobile::OrderedSourceGeneration::Counter(epoch_from_jlong(source_generation)),
        group_id: epoch_from_jlong(group_id),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestOrderedWakeFailure,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestOrderedWakeFailureForBinding,
    "mobile.ingress.ordered-wake-failure",
    (source_generation: jlong, group_id: jlong),
    |_env| Ok(RadioIngress::OrderedWakeFailure {
        generation: ubm_mobile::OrderedSourceGeneration::Counter(epoch_from_jlong(source_generation)),
        group_id: epoch_from_jlong(group_id),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdvertisement,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdvertisementForBinding,
    "mobile.ingest.advertisement",
    (peer_id: JString<'caller>, address: JString<'caller>, local_name: JString<'caller>, rssi: jint, tx_power: jint, service_uuids: JObjectArray<'caller, JString<'caller>>, company_ids: JIntArray<'caller>, manufacturer_payloads: JObjectArray<'caller, JByteArray<'caller>>, service_data_uuids: JObjectArray<'caller, JString<'caller>>, service_data_payloads: JObjectArray<'caller, JByteArray<'caller>>, connectable: jint, solicited_service_uuids: JObjectArray<'caller, JString<'caller>>, overflow_service_uuids: JObjectArray<'caller, JString<'caller>>, appearance: jint, raw_record: JByteArray<'caller>, cached_name: JString<'caller>, capture_timestamp_ms: jlong),
    |env| {
        const OP: &str = "mobile.ingest.advertisement";
        let company_ids = read_ints(env, &company_ids)?;
        let manufacturer_payloads = read_byte_arrays(env, &manufacturer_payloads, OP)?;
        same_length(OP, &[company_ids.len(), manufacturer_payloads.len()])?;
        let service_data_uuids = read_required_texts(env, &service_data_uuids, OP)?;
        let service_data_payloads = read_byte_arrays(env, &service_data_payloads, OP)?;
        same_length(OP, &[service_data_uuids.len(), service_data_payloads.len()])?;
        Ok(RadioIngress::Advertisement(Advertisement {
            cached_name: read_opt_text(env, &cached_name, OP)?,
            capture_timestamp_ms: if capture_timestamp_ms == -1 { None } else { Some(u64::try_from(capture_timestamp_ms).map_err(|_| invalid(OP, "capture timestamp must be non-negative or absent"))?) },
            peer_id: read_text(env, &peer_id, OP)?,
            address: read_opt_text(env, &address, OP)?,
            local_name: read_opt_text(env, &local_name, OP)?,
            rssi: optional_i16(rssi, OP)?,
            tx_power_level: optional_i16(tx_power, OP)?,
            service_uuids: read_required_texts(env, &service_uuids, OP)?,
            manufacturer_data: company_ids
                .into_iter()
                .zip(manufacturer_payloads)
                .map(|(company_id, payload)| {
                    u16::try_from(company_id)
                        .map(|company_id| ManufacturerData { company_id, payload })
                        .map_err(|_| invalid(OP, "company id out of range"))
                })
                .collect::<MobileResult<Vec<_>>>()?,
            service_data: service_data_uuids
                .into_iter()
                .zip(service_data_payloads)
                .map(|(uuid, payload)| ServiceData { uuid, payload })
                .collect(),
            connectable: match connectable {
                -1 => None,
                0 => Some(false),
                1 => Some(true),
                _ => return Err(invalid(OP, "connectable must be -1, 0 or 1")),
            },
            solicited_service_uuids: read_opt_texts(env, &solicited_service_uuids, OP)?,
            overflow_service_uuids: read_opt_texts(env, &overflow_service_uuids, OP)?,
            appearance: optional_int(appearance)
                .map(|value| {
                    u16::try_from(value).map_err(|_| invalid(OP, "appearance out of range"))
                })
                .transpose()?,
            raw_record: if raw_record.as_raw().is_null() {
                None
            } else {
                Some(read_bytes(env, &raw_record, OP)?)
            },
        }))
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdvertisementV2,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdvertisementV2ForBinding,
    "mobile.ingest.advertisement",
    (peer_id: JString<'caller>, address: JString<'caller>, local_name: JString<'caller>, rssi: jint, tx_power: jint, service_uuids: JObjectArray<'caller, JString<'caller>>, company_ids: JIntArray<'caller>, manufacturer_payloads: JObjectArray<'caller, JByteArray<'caller>>, service_data_uuids: JObjectArray<'caller, JString<'caller>>, service_data_payloads: JObjectArray<'caller, JByteArray<'caller>>, connectable: jint, solicited_service_uuids: JObjectArray<'caller, JString<'caller>>, overflow_service_uuids: JObjectArray<'caller, JString<'caller>>, appearance: jint, raw_record: JByteArray<'caller>, cached_name: JString<'caller>, capture_timestamp_ms: jlong, scan_identity: JString<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.advertisement";
        let company_ids = read_ints(env, &company_ids)?;
        let manufacturer_payloads = read_byte_arrays(env, &manufacturer_payloads, OP)?;
        same_length(OP, &[company_ids.len(), manufacturer_payloads.len()])?;
        let service_data_uuids = read_required_texts(env, &service_data_uuids, OP)?;
        let service_data_payloads = read_byte_arrays(env, &service_data_payloads, OP)?;
        same_length(OP, &[service_data_uuids.len(), service_data_payloads.len()])?;
        Ok(RadioIngress::AdvertisementScoped {
            scan_request_id: scan_identity_id(env, &scan_identity, OP)?,
            advertisement: Advertisement {
            cached_name: read_opt_text(env, &cached_name, OP)?,
            capture_timestamp_ms: if capture_timestamp_ms == -1 { None } else { Some(u64::try_from(capture_timestamp_ms).map_err(|_| invalid(OP, "capture timestamp must be non-negative or absent"))?) },
            peer_id: read_text(env, &peer_id, OP)?,
            address: read_opt_text(env, &address, OP)?,
            local_name: read_opt_text(env, &local_name, OP)?,
            rssi: optional_i16(rssi, OP)?,
            tx_power_level: optional_i16(tx_power, OP)?,
            service_uuids: read_required_texts(env, &service_uuids, OP)?,
            manufacturer_data: company_ids
                .into_iter()
                .zip(manufacturer_payloads)
                .map(|(company_id, payload)| {
                    u16::try_from(company_id)
                        .map(|company_id| ManufacturerData { company_id, payload })
                        .map_err(|_| invalid(OP, "company id out of range"))
                })
                .collect::<MobileResult<Vec<_>>>()?,
            service_data: service_data_uuids
                .into_iter()
                .zip(service_data_payloads)
                .map(|(uuid, payload)| ServiceData { uuid, payload })
                .collect(),
            connectable: match connectable {
                -1 => None,
                0 => Some(false),
                1 => Some(true),
                _ => return Err(invalid(OP, "connectable must be -1, 0 or 1")),
            },
            solicited_service_uuids: read_opt_texts(env, &solicited_service_uuids, OP)?,
            overflow_service_uuids: read_opt_texts(env, &overflow_service_uuids, OP)?,
            appearance: optional_int(appearance)
                .map(|value| {
                    u16::try_from(value).map_err(|_| invalid(OP, "appearance out of range"))
                })
                .transpose()?,
            raw_record: if raw_record.as_raw().is_null() {
                None
            } else {
                Some(read_bytes(env, &raw_record, OP)?)
            },
        }})
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestConnection,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestConnectionForBinding,
    "mobile.ingest.connection",
    (peer_id: JString<'caller>, connected: jboolean, status: jint),
    |env| Ok(RadioIngress::Connection {
        peer_id: read_text(env, &peer_id, "mobile.ingest.connection")?,
        connected,
        status: optional_int(status),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestConnectionForGeneration,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestConnectionForGenerationForBinding,
    "mobile.ingest.connection-scoped",
    (peer_id: JString<'caller>, expected_generation: JString<'caller>, connected: jboolean, status: jint),
    |env| {
        const OP: &str = "mobile.ingest.connection-scoped";
        let expected_generation = read_text(env, &expected_generation, OP)?;
        if expected_generation.is_empty() {
            return Err(invalid(OP, "expected generation must not be empty"));
        }
        Ok(RadioIngress::ConnectionScoped {
            peer_id: read_text(env, &peer_id, OP)?,
            expected_generation,
            connected,
            status: optional_int(status),
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestServicesChanged,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestServicesChangedForBinding,
    "mobile.ingest.services-changed",
    (peer_id: JString<'caller>),
    |env| Ok(RadioIngress::ServicesChanged {
        peer_id: read_text(env, &peer_id, "mobile.ingest.services-changed")?,
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestNotification,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestNotificationForBinding,
    "mobile.ingest.notification",
    (peer_id: JString<'caller>, service_uuid: JString<'caller>, service_occurrence: jlong, characteristic_uuid: JString<'caller>, characteristic_occurrence: jlong, epoch: jlong, value: JByteArray<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.notification";
        Ok(RadioIngress::Notification {
            instance: instance(
                env,
                &peer_id,
                &service_uuid,
                service_occurrence,
                &characteristic_uuid,
                characteristic_occurrence,
                OP,
            )?,
            epoch: epoch_from_jlong(epoch),
            value: read_bytes(env, &value, OP)?,
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestNotificationSourceFailed,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestNotificationSourceFailedForBinding,
    "mobile.ingest.notification-source-failed",
    (peer_id: JString<'caller>, service_uuid: JString<'caller>, service_occurrence: jlong, characteristic_uuid: JString<'caller>, characteristic_occurrence: jlong, epoch: jlong, kind: JString<'caller>, gatt_status: jint, detail: JString<'caller>, dispatched: jboolean, native_code: JString<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.notification-source-failed";
        Ok(RadioIngress::NotificationSourceFailed {
            instance: instance(env, &peer_id, &service_uuid, service_occurrence, &characteristic_uuid, characteristic_occurrence, OP)?,
            epoch: epoch_from_jlong(epoch),
            failure: PlatformFailure {
                kind: parse_enum(&read_text(env, &kind, OP)?, FailureKind::parse, OP)?,
                gatt_status: optional_int(gatt_status), native_domain: None, native_code: None,
                native_name: read_opt_text(env, &native_code, OP)?.filter(|code| !code.is_empty()),
                detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(), dispatched,
            },
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdapterState,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdapterStateForBinding,
    "mobile.ingest.adapter-state",
    (availability: JString<'caller>, authorization: JString<'caller>, power: JString<'caller>, safe_reason: JString<'caller>),
    |env| Ok(RadioIngress::AdapterState(adapter_snapshot(
        env,
        &availability,
        &authorization,
        &power,
        &safe_reason,
        "mobile.ingest.adapter-state"
    )?))
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailed,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailedForBinding,
    "mobile.ingest.scan-failed",
    (detail: JString<'caller>),
    |env| Ok(RadioIngress::ScanFailed {
        detail: read_opt_text(env, &detail, "mobile.ingest.scan-failed")?.unwrap_or_default(),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailedV2,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailedV2ForBinding,
    "mobile.ingest.scan-failed",
    (detail: JString<'caller>, scan_identity: JString<'caller>),
    |env| Ok(RadioIngress::ScanFailedScoped {
        scan_request_id: scan_identity_id(env, &scan_identity, "mobile.ingest.scan-failed")?,
        detail: read_opt_text(env, &detail, "mobile.ingest.scan-failed")?.unwrap_or_default(),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailedV3,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailedV3ForBinding,
    "mobile.ingest.scan-failed",
    (detail: JString<'caller>, scan_identity: JString<'caller>, error_code: jint),
    |env| Ok(RadioIngress::ScanFailedAndroid {
        scan_request_id: scan_identity_id(env, &scan_identity, "mobile.ingest.scan-failed")?,
        detail: read_opt_text(env, &detail, "mobile.ingest.scan-failed")?.unwrap_or_default(),
        native_code: error_code,
    })
);

// Opt-in typed response for a scan failure. The legacy integer entrypoints
// retain their existing ABI; callers of this entrypoint own the input until
// the response says accepted and can retry the original callback on refusal.
fn scan_failure_evidence_response(
    env: &mut Env,
    host: &MobileHost,
    detail: &JString,
    scan_identity: &JString,
    native_code: jint,
) -> MobileResult<jstring> {
    const OP: &str = "mobile.ingest.scan-failed-evidence";
    let ingress = RadioIngress::ScanFailedAndroid {
        scan_request_id: scan_identity_id(env, scan_identity, OP)?,
        detail: read_opt_text(env, detail, OP)?.unwrap_or_default(),
        native_code,
    };
    let result = scan_ingress_result(host.ingest(ingress)).to_string();
    Ok(env.new_string(result)?.into_raw())
}

// -- trusted scan-failure recovery --------------------------------------
//
// These entries are intentionally absent from the TurboModule.  Android's
// process host calls them only after it has installed its app-private
// recording directory and accepted a recording id under native policy.  The
// strings below are *handle names*, not durable capabilities: all authority,
// outbox keys and cleanup proofs remain in Rust.  Keeping the table in
// HostSlot also makes every handle fail closed after binding retirement.

const MAX_SCAN_RECOVERY_RAW_PAGE_BYTES: usize = 64 * 1024;

fn terminal_evidence_recovery_handles_mut(
    slot: &mut HostSlot,
    binding: jlong,
) -> MobileResult<&mut TerminalEvidenceRecoveryHandles> {
    if !slot.bindings.contains_key(&binding) {
        return Err(invalid(
            "terminal-evidence-recovery",
            "unknown or retired binding",
        ));
    }
    let epoch = *slot
        .terminal_evidence_recovery_epochs
        .entry(binding)
        .or_insert(1);
    Ok(slot
        .terminal_evidence_recovery
        .entry(binding)
        .or_insert_with(|| TerminalEvidenceRecoveryHandles {
            epoch,
            ..TerminalEvidenceRecoveryHandles::default()
        }))
}

fn terminal_evidence_recovery_id(
    handles: &mut TerminalEvidenceRecoveryHandles,
    binding: jlong,
    kind: &str,
) -> MobileResult<String> {
    handles.next = handles.next.checked_add(1).ok_or_else(|| {
        invalid(
            "terminal-evidence-recovery.handle",
            "handle namespace exhausted",
        )
    })?;
    Ok(format!(
        "terminal-evidence-recovery-{kind}-{binding}-{}-{}",
        handles.epoch, handles.next
    ))
}

fn recovery_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn scan_recovery_handles_mut(
    slot: &mut HostSlot,
    binding: jlong,
) -> MobileResult<&mut ScanRecoveryHandles> {
    if !slot.bindings.contains_key(&binding) {
        return Err(invalid(
            "scan.failure-recovery",
            "unknown or retired binding",
        ));
    }
    let epoch = *slot.scan_recovery_epochs.entry(binding).or_insert(1);
    Ok(slot
        .scan_recovery
        .entry(binding)
        .or_insert_with(|| ScanRecoveryHandles {
            epoch,
            ..ScanRecoveryHandles::default()
        }))
}

/// Revoke all process-local recovery authority for one binding. Selection
/// changes call this under their native-host lifecycle fence before exposing a
/// new generation; every other recovery entry takes the same slot lock.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRevokeForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
) {
    unowned_env
        .with_env(|_| -> MobileResult<()> {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !slot.bindings.contains_key(&binding) {
                return Err(invalid(
                    "scan.failure-recovery.revoke",
                    "unknown or retired binding",
                ));
            }
            slot.scan_recovery.remove(&binding);
            let epoch = slot.scan_recovery_epochs.entry(binding).or_insert(1);
            *epoch = epoch.checked_add(1).ok_or_else(|| {
                invalid("scan.failure-recovery.revoke", "recovery epoch exhausted")
            })?;
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.revoke")
}

fn scan_recovery_response(env: &mut Env, value: serde_json::Value) -> MobileResult<jstring> {
    Ok(env.new_string(value.to_string())?.into_raw())
}

#[cfg(ubm_jni_roundtrip)]
struct TestRecoveryWake;
#[cfg(ubm_jni_roundtrip)]
impl TestWakeSink for TestRecoveryWake {
    fn wake(&self, _: u64) {}
}

/// Seeds exact staged and committed rows in the host-selected SQLite ledger
/// for the JVM foreign-boundary recovery harness. The production recovery
/// calls below still issue their opaque aliases, pagination, reads and ACKs
/// through JNI; this only supplies durable terminal fixtures that a radio
/// callback cannot deterministically create on a JVM.
#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestTerminalEvidenceRecovery_nativeTestSeedTerminalEvidenceForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    producer: JString<'caller>,
    manager_id: JString<'caller>,
    operation_id: JString<'caller>,
    incarnation: JString<'caller>,
    bytes: JByteArray<'caller>,
    committed: jboolean,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "terminal-evidence-recovery.test-seed";
            let host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            host.seed_terminal_evidence_for_test(
                &read_text(env, &producer, OP)?,
                &read_text(env, &manager_id, OP)?,
                &read_text(env, &operation_id, OP)?,
                &read_text(env, &incarnation, OP)?,
                &read_bytes(env, &bytes, OP)?,
                committed,
            )
            .map_err(MobileError::from)
        })
        .resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.test-seed")
}

/// Builds a real durable recovery owner for the JVM production-boundary
/// harness. This symbol is compiled only by `run_scan_failure_evidence.py`'s
/// `--cfg ubm_jni_roundtrip` build and is never part of the normal Android
/// binary or TurboModule surface.
#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestScanFailureEvidence_nativeTestSeedScanFailureRecoveryForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    recording_id: JString<'caller>,
    directory: JString<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "scan.failure-recovery.test-seed";
            let recording_id = read_text(env, &recording_id, OP)?;
            let directory = read_text(env, &directory, OP)?;
            let _host = binding_host(binding).ok_or_else(|| invalid(OP, "unknown binding"))?;
            let path = std::path::Path::new(&directory).join(format!("{recording_id}.sqlite"));
            let quota = JournalQuota {
                max_bytes: 1 << 20,
                max_records: 64,
            };
            // Create the ordinary private recording once, then reopen it via
            // the configured production registry below. The test-only path is
            // a directory supplied by the JVM's temporary sandbox, never a
            // renderer input or a production JNI entrypoint.
            let journal = Arc::new(
                ContinuationJournal::open(
                    &path,
                    &recording_id,
                    &serde_json::json!({"version":1,"source":"jni-roundtrip"}),
                    quota,
                )
                .map_err(|_| invalid(OP, "could not create test recording"))?,
            );
            drop(journal);
            let journal = ubm_mobile::continuation::process_recording_registry()
                .get(&recording_id)
                .map_err(|_| invalid(OP, "test recording is unavailable"))?;
            let outbox = TestOutbox::new(0x5245_434f_5645_5259, Arc::new(TestRecoveryWake));
            outbox
                .configure_scan_failure_recovery_authority([0x51; 32])
                .map_err(|_| invalid(OP, "could not configure recovery authority"))?;
            outbox
                .attach_journal(journal, serde_json::json!({"kind":"jni-roundtrip"}))
                .map_err(|_| invalid(OP, "could not attach test journal"))?;
            let member = ScanFailureMember {
                recipient: [0x11; 16],
                delivery: [0x22; 16],
            };
            let seed_count = if recording_id == "fixture-cleanup-batch" {
                2
            } else {
                1
            };
            for _ in 0..seed_count {
                let raw_fixture =
                    recording_id == "fixture-raw" || recording_id == "fixture-cleanup-batch";
                let mut reservation = outbox
                    .reserve_scan_failure(ScanFailureLimits {
                        evidence: EvidenceLimits {
                            max_depth: 8,
                            max_tokens: 16,
                            // A deliberately impossible prepared encoding
                            // leaves the source captured, durable and
                            // unprepared for the raw-recovery harness.
                            max_encoded_bytes: if raw_fixture { 1 } else { 4096 },
                            max_chunks: 8,
                            chunk_bytes: 64,
                        },
                        max_recipients: 1,
                        max_bookkeeping_bytes: 4096,
                    })
                    .map_err(|_| invalid(OP, "could not reserve test recovery"))?;
                let sequence = TEST_RECOVERY_OCCURRENCE.fetch_add(1, Ordering::Relaxed);
                let mut occurrence = Some(ScanFailureOccurrence::new(
                    ScanFailureIdentity {
                        host: [1; 16],
                        scan: [2; 16],
                        start: [3; 16],
                        source_generation: [4; 16],
                        occurrence: sequence
                            .to_be_bytes()
                            .repeat(2)
                            .try_into()
                            .map_err(|_| invalid(OP, "test occurrence encoding failed"))?,
                    },
                    vec![member],
                    Arc::new(EvidenceSource::new(vec![
                        EvidenceToken::Array(3),
                        EvidenceToken::Bytes(vec![0x5a; 128]),
                        EvidenceToken::Null,
                        EvidenceToken::Null,
                    ])),
                ));
                let committed = outbox.commit_scan_failure(&mut reservation, &mut occurrence);
                if raw_fixture {
                    if committed.is_ok() || occurrence.is_some() {
                        return Err(invalid(OP, "raw test recovery unexpectedly prepared"));
                    }
                } else {
                    committed.map_err(|_| invalid(OP, "could not persist test recovery"))?;
                }
                outbox
                    .finish_scan_failure_producer(&mut reservation)
                    .map_err(|_| invalid(OP, "could not finish test recovery"))?;
            }
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.test-seed")
}

fn scan_recovery_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn scan_recovery_identity_json(
    identity: ubm_core::oversized_evidence::ScanFailureIdentity,
) -> serde_json::Value {
    serde_json::json!({
        "host": scan_recovery_hex(&identity.host),
        "scan": scan_recovery_hex(&identity.scan),
        "start": scan_recovery_hex(&identity.start),
        "sourceGeneration": scan_recovery_hex(&identity.source_generation),
        "occurrence": scan_recovery_hex(&identity.occurrence),
    })
}

fn scan_recovery_canonical_usize(
    text: &str,
    operation: &'static str,
    name: &'static str,
) -> MobileResult<usize> {
    let parsed = text
        .parse::<u64>()
        .map_err(|_| invalid(operation, "raw token numeric input must be canonical u64"))?;
    if parsed.to_string() != text {
        return Err(invalid(
            operation,
            "raw token numeric input must be canonical u64",
        ));
    }
    usize::try_from(parsed).map_err(|_| invalid(operation, name))
}

type ScanRecoveryRawTokenFields = (
    &'static str,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
);

fn scan_recovery_raw_token_object<'local>(
    env: &mut Env<'local>,
    token: ScanFailureRawToken,
) -> MobileResult<JObject<'local>> {
    let (kind, value, total, count, bytes): ScanRecoveryRawTokenFields = match token {
        ScanFailureRawToken::Null => ("null", None, None, None, None),
        ScanFailureRawToken::Bool(value) => ("bool", Some(value.to_string()), None, None, None),
        ScanFailureRawToken::I64(value) => ("i64", Some(value.to_string()), None, None, None),
        ScanFailureRawToken::U64(value) => ("u64", Some(value.to_string()), None, None, None),
        ScanFailureRawToken::F64Bits(value) => {
            ("f64Bits", Some(value.to_string()), None, None, None)
        }
        ScanFailureRawToken::Text { total, bytes } => {
            ("text", None, Some(total.to_string()), None, Some(bytes))
        }
        ScanFailureRawToken::Bytes { total, bytes } => {
            ("bytes", None, Some(total.to_string()), None, Some(bytes))
        }
        ScanFailureRawToken::Map(count) => ("map", None, None, Some(count.to_string()), None),
        ScanFailureRawToken::Array(count) => ("array", None, None, Some(count.to_string()), None),
    };
    let class = env.find_class(JNIString::from(
        "com/ubm/core/MobileCoreBridge$ScanFailureRecoveryRawToken",
    ))?;
    let kind = JObject::from(env.new_string(kind)?);
    let value = match value {
        Some(value) => JObject::from(env.new_string(value)?),
        None => JObject::null(),
    };
    let total = match total {
        Some(total) => JObject::from(env.new_string(total)?),
        None => JObject::null(),
    };
    let count = match count {
        Some(count) => JObject::from(env.new_string(count)?),
        None => JObject::null(),
    };
    let bytes = match bytes {
        Some(bytes) => JObject::from(env.byte_array_from_slice(&bytes)?),
        None => JObject::null(),
    };
    env.new_object(
        class,
        MethodSignature::from(&RuntimeMethodSignature::from_str(
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[B)V",
        )?),
        &[
            JValue::Object(&kind),
            JValue::Object(&value),
            JValue::Object(&total),
            JValue::Object(&count),
            JValue::Object(&bytes),
        ],
    )
    .map_err(Into::into)
}

/// Test-harness-only controls live in the JVM harness class rather than the
/// shipped bridge. They are compiled solely with `ubm_jni_roundtrip` and
/// cannot be linked from a production Android artifact.
#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestScanFailureEvidence_nativeTestSetRecoveryHandleLimit(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
    limit: jint,
) {
    unowned_env
        .with_env(|_| -> MobileResult<()> {
            let limit = usize::try_from(limit).map_err(|_| {
                invalid("scan.failure-recovery.test-limit", "limit must be positive")
            })?;
            if limit == 0 {
                return Err(invalid(
                    "scan.failure-recovery.test-limit",
                    "limit must be positive",
                ));
            }
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            if handles.live_handle_count() > limit {
                return Err(invalid(
                    "scan.failure-recovery.test-limit",
                    "limit below live handle count",
                ));
            }
            handles.limit = limit;
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.test-limit")
}

#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestScanFailureEvidence_nativeTestRecoveryLeaseCount(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
) -> jint {
    unowned_env
        .with_env(|_| -> MobileResult<jint> {
            let host = binding_host(binding)
                .ok_or_else(|| invalid("scan.failure-recovery.test-count", "unknown binding"))?;
            jint::try_from(host.scan_failure_recovery_registry().lease_count())
                .map_err(|_| invalid("scan.failure-recovery.test-count", "lease count overflow"))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.test-count")
}

#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestScanFailureEvidence_nativeTestRecoveryHandleCount(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
) -> jint {
    unowned_env
        .with_env(|_| -> MobileResult<jint> {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            jint::try_from(handles.live_handle_count())
                .map_err(|_| invalid("scan.failure-recovery.test-count", "handle count overflow"))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.test-count")
}

#[cfg(ubm_jni_roundtrip)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestScanFailureEvidence_nativeTestClearRecoveryCleanupAliases(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
) {
    unowned_env
        .with_env(|_| -> MobileResult<()> {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scan_recovery_handles_mut(&mut slot, binding)?
                .cleanup
                .clear();
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.test-clear-cleanup")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryAttachForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    recording_id: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "scan.failure-recovery.attach";
            let recording_id = read_text(env, &recording_id, OP)?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let host = slot
                .bindings
                .get(&binding)
                .cloned()
                .ok_or_else(|| invalid(OP, "unknown or retired binding"))?;
            let existing =
                scan_recovery_handles_mut(&mut slot, binding)?.lease_for_recording(&recording_id);
            if let Some(id) = existing {
                return scan_recovery_response(env, serde_json::json!({"leaseId": id}));
            }
            // Admission happens before touching the core registry. Otherwise a
            // rejected public alias would leave a hidden process lease alive.
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            handles.ensure_room(OP)?;
            let id = handles.next_id(binding, "lease")?;
            let lease = host
                .scan_failure_recovery_registry()
                .attach_trusted(&recording_id)?;
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            handles.leases.insert(id.clone(), lease);
            handles.leases_by_recording.insert(recording_id, id.clone());
            scan_recovery_response(env, serde_json::json!({"leaseId": id}))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.attach")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeTerminalEvidenceRecoveryOpenSelectedForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let host = slot.bindings.get(&binding).cloned().ok_or_else(|| {
                invalid(
                    "terminal-evidence-recovery.open",
                    "unknown or retired binding",
                )
            })?;
            let recovery = Arc::new(host.terminal_evidence_recovery_selected()?);
            let handles = terminal_evidence_recovery_handles_mut(&mut slot, binding)?;
            let id = terminal_evidence_recovery_id(handles, binding, "lease")?;
            handles.clear_lease();
            handles.lease_id = Some(id.clone());
            handles.lease = Some(recovery);
            scan_recovery_response(env, serde_json::json!({"leaseId":id}))
        })
        .resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.open")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeTerminalEvidenceRecoveryListForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
    lease_id: JString<'_>,
    max_items: jint,
    max_bytes: jlong,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        let lease_id = read_text(env, &lease_id, "terminal-evidence-recovery.list")?;
        let max_items = u32::try_from(max_items).map_err(|_| invalid("terminal-evidence-recovery.list", "max items must be positive"))?;
        let max_bytes = u64::try_from(max_bytes).map_err(|_| invalid("terminal-evidence-recovery.list", "max bytes must be positive"))?;
        let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = terminal_evidence_recovery_handles_mut(&mut slot, binding)?;
        if handles.lease_id.as_deref() != Some(lease_id.as_str()) { return Err(invalid("terminal-evidence-recovery.list", "unknown or revoked recovery lease")); }
        let lease = handles.lease.clone().ok_or_else(|| invalid("terminal-evidence-recovery.list", "unknown or revoked recovery lease"))?;
        let page = lease.list(max_items, max_bytes)?;
        let mut items = Vec::with_capacity(page.entries.len());
        for value in page.entries {
            let id = handles.entry_alias(binding, &value)?;
            let mut item = serde_json::json!({"handle":id,"producer":value.producer,"scope":{"managerId":value.scope.manager_id,"operationId":value.scope.operation_id,"incarnation":value.scope.incarnation},"state":match value.state { ubm_mobile::TerminalEvidenceRecoveryState::Staged=>"staged", ubm_mobile::TerminalEvidenceRecoveryState::Committed=>"committed"}});
            if let Some(object) = item.as_object_mut() {
                if let Some(reference) = value.reference {
                    object.insert("reference".into(), reference.into());
                }
                if let Some(length) = value.byte_length {
                    object.insert("byteLength".into(), length.to_string().into());
                }
                if let Some(digest) = value.digest {
                    object.insert("digest".into(), digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>().into());
                }
                if let Some(version) = value.format_version {
                    object.insert("formatVersion".into(), version.into());
                }
            }
            items.push(item);
        }
        scan_recovery_response(env, serde_json::json!({"items":items,"more":page.more}))
    }).resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.list")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeTerminalEvidenceRecoveryListWithStatusForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
    lease_id: JString<'_>,
    max_items: jint,
    max_bytes: jlong,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        let lease_id = read_text(env, &lease_id, "terminal-evidence-recovery.list-status")?;
        let max_items = u32::try_from(max_items).map_err(|_| invalid("terminal-evidence-recovery.list-status", "max items must be positive"))?;
        let max_bytes = u64::try_from(max_bytes).map_err(|_| invalid("terminal-evidence-recovery.list-status", "max bytes must be positive"))?;
        let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = terminal_evidence_recovery_handles_mut(&mut slot, binding)?;
        if handles.lease_id.as_deref() != Some(lease_id.as_str()) { return Err(invalid("terminal-evidence-recovery.list-status", "unknown or revoked recovery lease")); }
        let lease = handles.lease.clone().ok_or_else(|| invalid("terminal-evidence-recovery.list-status", "unknown or revoked recovery lease"))?;
        let page = lease.list_with_status(max_items, max_bytes)?;
        let mut items = Vec::with_capacity(page.entries.len());
        for value in page.entries {
            let id = handles.entry_alias(binding, &value)?;
            let mut item = serde_json::json!({"handle":id,"producer":value.producer,"scope":{"managerId":value.scope.manager_id,"operationId":value.scope.operation_id,"incarnation":value.scope.incarnation},"state":match value.state { ubm_mobile::TerminalEvidenceRecoveryState::Staged=>"staged", ubm_mobile::TerminalEvidenceRecoveryState::Committed=>"committed"}});
            if let Some(object) = item.as_object_mut() {
                if let Some(reference) = value.reference {
                    object.insert("reference".into(), reference.into());
                }
                if let Some(length) = value.byte_length {
                    object.insert("byteLength".into(), length.to_string().into());
                }
                if let Some(digest) = value.digest {
                    object.insert("digest".into(), digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>().into());
                }
                if let Some(version) = value.format_version {
                    object.insert("formatVersion".into(), version.into());
                }
            }
            items.push(item);
        }
        scan_recovery_response(env, serde_json::json!({"items":items,"more":page.more,"busyProducers":page.busy_producers}))
    }).resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.list-status")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeTerminalEvidenceRecoveryReadForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
    handle: JString<'_>,
    offset: JString<'_>,
    max_bytes: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let handle = read_text(env, &handle, "terminal-evidence-recovery.read")?;
            let offset = read_text(env, &offset, "terminal-evidence-recovery.read")?
                .parse::<u64>()
                .map_err(|_| {
                    invalid(
                        "terminal-evidence-recovery.read",
                        "offset must be canonical unsigned decimal",
                    )
                })?;
            let max = u32::try_from(max_bytes).map_err(|_| {
                invalid(
                    "terminal-evidence-recovery.read",
                    "max bytes must be positive",
                )
            })?;
            let slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = slot
                .terminal_evidence_recovery
                .get(&binding)
                .ok_or_else(|| invalid("terminal-evidence-recovery.read", "unknown binding"))?;
            let lease = handles.lease.clone().ok_or_else(|| {
                invalid("terminal-evidence-recovery.read", "unknown recovery lease")
            })?;
            let entry = handles.entries.get(&handle).ok_or_else(|| {
                invalid("terminal-evidence-recovery.read", "unknown recovery entry")
            })?;
            let page = lease.read(entry, offset, max)?;
            scan_recovery_response(
                env,
                serde_json::json!({"bytesBase64":recovery_base64(&page.bytes),"more":!page.done}),
            )
        })
        .resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.read")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeTerminalEvidenceRecoveryAcknowledgeForBinding(
    mut unowned_env: EnvUnowned<'_>,
    _class: JClass<'_>,
    binding: jlong,
    handle: JString<'_>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let handle = read_text(env, &handle, "terminal-evidence-recovery.acknowledge")?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = terminal_evidence_recovery_handles_mut(&mut slot, binding)?;
            if handles.acknowledged.contains(&handle) {
                return scan_recovery_response(env, serde_json::json!({}));
            }
            let lease = handles.lease.clone().ok_or_else(|| {
                invalid(
                    "terminal-evidence-recovery.acknowledge",
                    "unknown recovery lease",
                )
            })?;
            let entry = handles.entries.get(&handle).cloned().ok_or_else(|| {
                invalid(
                    "terminal-evidence-recovery.acknowledge",
                    "unknown recovery entry",
                )
            })?;
            lease.acknowledge(&entry)?;
            handles.retain_acknowledgement(handle);
            scan_recovery_response(env, serde_json::json!({}))
        })
        .resolve_with::<ThrowMobile, _>(|| "terminal-evidence-recovery.acknowledge")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryOwnersForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    lease_id: JString<'caller>,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        let lease_id = read_text(env, &lease_id, "scan.failure-recovery.owners")?;
        let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = scan_recovery_handles_mut(&mut slot, binding)?;
        let lease = handles.lease(&lease_id)?;
        let owners = lease.owners();
        let aliases = handles.issue_owner_batch(
            binding,
            &lease_id,
            &owners.iter().map(|owner| owner.key).collect::<Vec<_>>(),
        )?;
        let mut values = Vec::new();
        for (owner, id) in owners.into_iter().zip(aliases) {
            let value = serde_json::json!({"ownerId": id, "prepared": owner.prepared, "producerFinished": owner.producer_finished, "rawRetired": owner.raw_retired, "rawReleased": owner.raw_released, "memberCount": owner.member_count, "released": owner.released.is_some()});
            values.push(value);
        }
        scan_recovery_response(env, serde_json::json!({"owners": values}))
    }).resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.owners")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryHeadForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
    index: jint,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        const OP: &str = "scan.failure-recovery.head";
        let owner_id = read_text(env, &owner_id, OP)?;
        let index = usize::try_from(index).map_err(|_| invalid(OP, "member index must be non-negative"))?;
        let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = scan_recovery_handles_mut(&mut slot, binding)?;
        let (lease, owner) = handles.owner(&owner_id)?;
        let member = lease.member(&owner, index)?;
        let Some(head) = lease.head(member)? else { return scan_recovery_response(env, serde_json::json!({"head": serde_json::Value::Null})); };
        let lease_id = handles.owners.get(&owner_id).ok_or_else(|| invalid(OP, "recovery owner was revoked"))?.0.clone();
        let id = match handles.head_alias(&lease_id, &head) {
            Some(id) => id,
            None => {
                handles.ensure_room(OP)?;
                let id = handles.next_id(binding, "head")?;
                handles.heads.insert(id.clone(), (lease_id, head));
                id
            }
        };
        let value = serde_json::json!({"headId": id, "protocol": head.evidence.protocol, "representation": head.evidence.representation, "encodedLength": head.evidence.encoded_length.to_string(), "chunkBytes": head.evidence.chunk_bytes});
        scan_recovery_response(env, serde_json::json!({"head": value}))
    }).resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.head")
}

/// Resolve a member only through an opaque, binding-local owner handle.  The
/// returned identifiers are metadata, not the pending-owner capability; they
/// intentionally match the desktop recovery representation.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryMemberForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
    index: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "scan.failure-recovery.member";
            let owner_id = read_text(env, &owner_id, OP)?;
            let index = usize::try_from(index)
                .map_err(|_| invalid(OP, "member index must be non-negative"))?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, owner) = handles.owner(&owner_id)?;
            let member = lease.member(&owner, index)?;
            scan_recovery_response(
                env,
                serde_json::json!({"member": {
                    "recipient": scan_recovery_hex(&member.recipient),
                    "delivery": scan_recovery_hex(&member.delivery),
                }}),
            )
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.member")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryReadForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    head_id: JString<'caller>,
    offset: JString<'caller>,
) -> jstring {
    unowned_env.with_env(|env| -> MobileResult<jstring> {
        const OP: &str = "scan.failure-recovery.read";
        let head_id = read_text(env, &head_id, OP)?;
        let offset = read_text(env, &offset, OP)?.parse::<u64>().map_err(|_| invalid(OP, "offset must be a canonical u64"))?;
        let mut slot = host_slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = scan_recovery_handles_mut(&mut slot, binding)?;
        let (lease, head) = handles.head(&head_id)?;
        let page = lease.read(&head, offset)?;
        if page.bytes.len() > MAX_SCAN_RECOVERY_RAW_PAGE_BYTES { return Err(invalid(OP, "native page exceeded bounded byte contract")); }
        // Bytes deliberately travel through the companion `byte[]` entry
        // below.  Serializing them as JSON numbers corrupts the public binary
        // contract (and inflates a 64 KiB page several times over).
        scan_recovery_response(env, serde_json::json!({"offset": page.offset.to_string(), "nextOffset": page.next_offset.to_string(), "byteLength": page.bytes.len(), "done": page.done}))
    }).resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.read")
}

/// Atomic metadata-and-payload page for the JSI recovery provider.  Unlike the
/// compatibility metadata/byte[] pair below, this takes the outbox read once
/// while the binding handle table is fenced, so an ACK, release, reconnect or
/// teardown cannot pair metadata from one durable state with bytes from
/// another.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryReadPageForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    head_id: JString<'caller>,
    offset: JString<'caller>,
) -> JObject<'caller> {
    unowned_env
        .with_env(|env| -> MobileResult<JObject<'caller>> {
            const OP: &str = "scan.failure-recovery.read";
            let head_id = read_text(env, &head_id, OP)?;
            let offset = read_text(env, &offset, OP)?
                .parse::<u64>()
                .map_err(|_| invalid(OP, "offset must be a canonical u64"))?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, head) = handles.head(&head_id)?;
            let page = lease.read(&head, offset)?;
            if page.bytes.len() > MAX_SCAN_RECOVERY_RAW_PAGE_BYTES {
                return Err(invalid(OP, "native page exceeded bounded byte contract"));
            }
            let class = env.find_class(JNIString::from(
                "com/ubm/core/MobileCoreBridge$ScanFailureRecoveryPage",
            ))?;
            let page_offset = JObject::from(env.new_string(page.offset.to_string())?);
            let next_offset = JObject::from(env.new_string(page.next_offset.to_string())?);
            let bytes = JObject::from(env.byte_array_from_slice(&page.bytes)?);
            env.new_object(
                class,
                MethodSignature::from(&RuntimeMethodSignature::from_str(
                    "(Ljava/lang/String;Ljava/lang/String;[BZ)V",
                )?),
                &[
                    JValue::Object(&page_offset),
                    JValue::Object(&next_offset),
                    JValue::Object(&bytes),
                    JValue::Bool(page.done as jboolean),
                ],
            )
            .map_err(Into::into)
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.read")
}

/// The binary half of `nativeScanFailureRecoveryReadForBinding`.  Metadata and
/// payload intentionally use separate JNI values: Java can convert this fresh
/// `byte[]` directly to the neutral `Uint8Array` contract without putting
/// arbitrary numeric arrays through JSON.  Both calls are read-only and the
/// `headId`/`offset` pair is binding-scoped, so a stale or foreign handle fails
/// exactly as the metadata route does.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryReadBytesForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    head_id: JString<'caller>,
    offset: JString<'caller>,
) -> jbyteArray {
    unowned_env
        .with_env(|env| -> MobileResult<jbyteArray> {
            const OP: &str = "scan.failure-recovery.read";
            let head_id = read_text(env, &head_id, OP)?;
            let offset = read_text(env, &offset, OP)?
                .parse::<u64>()
                .map_err(|_| invalid(OP, "offset must be a canonical u64"))?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, head) = handles.head(&head_id)?;
            let page = lease.read(&head, offset)?;
            if page.bytes.len() > MAX_SCAN_RECOVERY_RAW_PAGE_BYTES {
                return Err(invalid(OP, "native page exceeded bounded byte contract"));
            }
            env.byte_array_from_slice(&page.bytes)
                .map(|array| array.into_raw())
                .map_err(|_| invalid(OP, "could not allocate page bytes"))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.read")
}

fn scan_recovery_head_control(
    binding: jlong,
    head_id: &str,
    operation: &str,
) -> MobileResult<Option<String>> {
    let mut slot = host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let handles = scan_recovery_handles_mut(&mut slot, binding)?;
    let (lease, head) = handles.head(head_id)?;
    match operation {
        "acknowledge" => {
            lease.acknowledge(&head)?;
            Ok(None)
        }
        "retire" => {
            lease.retire(&head)?;
            Ok(None)
        }
        "release" => {
            lease.release(&head)?;
            Ok(None)
        }
        "finalize" => {
            let lease_id = handles
                .heads
                .get(head_id)
                .ok_or_else(|| {
                    invalid(
                        "scan.failure-recovery.finalize",
                        "recovery head was revoked",
                    )
                })?
                .0
                .clone();
            // The durable finalization is irreversible. Reserve the alias and
            // its identifier first so a full process table cannot make the
            // native transition appear to fail without returning its receipt.
            handles.reserve_head_replacement("scan.failure-recovery.finalize")?;
            let id = handles.next_id(binding, "cleanup")?;
            let receipt = lease.finalize_release(&head)?;
            handles.cleanup.insert(id.clone(), (lease_id, receipt));
            // A finalized descriptor cannot be read again.  The durable
            // cleanup receipt remains explicitly retryable until retirement.
            handles.heads.remove(head_id);
            Ok(Some(id))
        }
        _ => Err(invalid(
            "scan.failure-recovery.head-control",
            "unsupported control",
        )),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryHeadControlForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    head_id: JString<'caller>,
    operation: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let head = read_text(env, &head_id, "scan.failure-recovery.head-control")?;
            let operation = read_text(env, &operation, "scan.failure-recovery.head-control")?;
            let receipt = scan_recovery_head_control(binding, &head, &operation)?;
            scan_recovery_response(env, serde_json::json!({"cleanupReceiptId": receipt}))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.head-control")
}

/// Advance the ordered raw-source side of a pending recovery owner. The owner
/// remains an opaque binding-local capability; raw transport values never
/// cross this control boundary.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRawControlForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
    operation: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "scan.failure-recovery.raw-control";
            let owner_id = read_text(env, &owner_id, OP)?;
            let operation = read_text(env, &operation, OP)?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, owner) = handles.owner(&owner_id)?;
            let cleanup = match operation.as_str() {
                "retire" => {
                    lease.raw_retire(&owner)?;
                    None
                }
                "release" => {
                    lease.raw_release(&owner)?;
                    None
                }
                "finalize" => {
                    let lease_id = handles
                        .owners
                        .get(&owner_id)
                        .ok_or_else(|| invalid(OP, "recovery owner was revoked"))?
                        .0
                        .clone();
                    handles.ensure_room(OP)?;
                    let id = handles.next_id(binding, "cleanup")?;
                    let receipt = lease.raw_finalize(&owner)?;
                    handles.cleanup.insert(id.clone(), (lease_id, receipt));
                    Some(id)
                }
                _ => return Err(invalid(OP, "unsupported raw control")),
            };
            scan_recovery_response(env, serde_json::json!({"cleanupReceiptId": cleanup}))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.raw-control")
}

/// Inspect raw durable evidence through a binding-local opaque owner handle.
/// The identity describes the retained occurrence but contains no recovery
/// capability or filesystem authority.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRawInfoForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "scan.failure-recovery.raw-info";
            let owner_id = read_text(env, &owner_id, OP)?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, owner) = handles.owner(&owner_id)?;
            let info = lease.raw_info(&owner)?;
            scan_recovery_response(
                env,
                serde_json::json!({
                    "identity": scan_recovery_identity_json(info.identity),
                    "memberCount": info.member_count,
                    "tokenCount": info.token_count,
                }),
            )
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.raw-info")
}

/// Resolve one retained raw member through an opaque owner.  This mirrors the
/// desktop raw-member representation and does not expose owner capability
/// bytes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRawMemberForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
    index: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "scan.failure-recovery.raw-member";
            let owner_id = read_text(env, &owner_id, OP)?;
            let index = usize::try_from(index)
                .map_err(|_| invalid(OP, "member index must be non-negative"))?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, owner) = handles.owner(&owner_id)?;
            let member = lease.raw_member(&owner, index)?;
            scan_recovery_response(
                env,
                serde_json::json!({"member": {
                    "recipient": scan_recovery_hex(&member.recipient),
                    "delivery": scan_recovery_hex(&member.delivery),
                }}),
            )
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.raw-member")
}

/// Return one raw token with a single bounded native read.  Text and bytes
/// remain byte arrays; all other values are represented exactly as canonical
/// decimal text or fixed kinds in the Java DTO.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRawTokenForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    owner_id: JString<'caller>,
    index: jint,
    offset: JString<'caller>,
    max_bytes: JString<'caller>,
) -> JObject<'caller> {
    unowned_env
        .with_env(|env| -> MobileResult<JObject<'caller>> {
            const OP: &str = "scan.failure-recovery.raw-token";
            let owner_id = read_text(env, &owner_id, OP)?;
            let index = usize::try_from(index)
                .map_err(|_| invalid(OP, "token index must be non-negative"))?;
            let offset = scan_recovery_canonical_usize(
                &read_text(env, &offset, OP)?,
                OP,
                "offset does not fit usize",
            )?;
            let max_bytes = scan_recovery_canonical_usize(
                &read_text(env, &max_bytes, OP)?,
                OP,
                "maxBytes does not fit usize",
            )?;
            if max_bytes == 0 || max_bytes > MAX_SCAN_RECOVERY_RAW_PAGE_BYTES {
                return Err(invalid(
                    OP,
                    "maxBytes must be within the bounded raw-token contract",
                ));
            }
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, owner) = handles.owner(&owner_id)?;
            let token = lease.raw_token(&owner, index, offset, max_bytes)?;
            scan_recovery_raw_token_object(env, token)
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.raw-token")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryPendingCleanupForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    lease_id: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let lease_id = read_text(env, &lease_id, "scan.failure-recovery.cleanup-receipts")?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let receipts = handles.lease(&lease_id)?.pending_cleanup_receipts()?;
            let ids = handles.issue_cleanup_batch(binding, &lease_id, &receipts)?;
            scan_recovery_response(env, serde_json::json!({"cleanupReceiptIds": ids}))
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.cleanup-receipts")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureRecoveryRetireCleanupForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    receipt_id: JString<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            let receipt_id = read_text(env, &receipt_id, "scan.failure-recovery.cleanup-retire")?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = scan_recovery_handles_mut(&mut slot, binding)?;
            let (lease, receipt) = handles.cleanup(&receipt_id)?;
            lease.retire_cleanup(&receipt)?;
            let lease_id = handles
                .cleanup
                .get(&receipt_id)
                .ok_or_else(|| {
                    invalid(
                        "scan.failure-recovery.cleanup-retire",
                        "cleanup receipt was revoked during retirement",
                    )
                })?
                .0
                .clone();
            handles.cleanup.remove(&receipt_id);
            handles.reclaim_terminal_lease(&lease_id);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "scan.failure-recovery.cleanup-retire")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureIngestEvidence<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    detail: JString<'caller>,
    scan_identity: JString<'caller>,
    native_code: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let host = require_legacy_session_host("mobile.ingest.scan-failed-evidence")?;
            scan_failure_evidence_response(env, &host, &detail, &scan_identity, native_code)
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.ingest.scan-failed-evidence")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeScanFailureIngestEvidenceForBinding<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
    detail: JString<'caller>,
    scan_identity: JString<'caller>,
    native_code: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            let host = binding_host(binding)
                .ok_or_else(|| invalid("mobile.ingest.scan-failed-evidence", "unknown binding"))?;
            scan_failure_evidence_response(env, &host, &detail, &scan_identity, native_code)
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.ingest.scan-failed-evidence")
}

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestSecurity,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestSecurityForBinding,
    "mobile.ingest.security",
    (peer_id: JString<'caller>, bond: JString<'caller>, encryption: JString<'caller>, authentication: JString<'caller>, secure_connections: JString<'caller>, pairing_possible: jint),
    |env| {
        const OP: &str = "mobile.ingest.security";
        Ok(RadioIngress::SecurityChanged {
            peer_id: read_text(env, &peer_id, OP)?,
            state: security_state(
                env,
                &bond,
                &encryption,
                &authentication,
                &secure_connections,
                pairing_possible,
                OP,
            )?,
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestSecurityFailure,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestSecurityFailureForBinding,
    "mobile.ingest.security-failed",
    (peer_id: JString<'caller>, kind: JString<'caller>, encryption_status: jint, detail: JString<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.security-failed";
        let status = optional_int(encryption_status);
        Ok(RadioIngress::SecurityFailed {
            peer_id: read_opt_text(env, &peer_id, OP)?,
            failure: PlatformFailure {
                kind: parse_enum(&read_text(env, &kind, OP)?, FailureKind::parse, OP)?,
                gatt_status: None,
                native_domain: status.map(|_| "android.bluetooth.HciEncryptionChange".into()),
                native_code: status.map(i64::from),
                native_name: Some(if status.is_some() { "ENCRYPTION_CHANGE_FAILED" } else { "encryption-observation-failed" }.into()),
                detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(), dispatched: false,
            },
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestRestored,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestRestoredForBinding,
    "mobile.ingest.restored",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, names: JObjectArray<'caller, JString<'caller>>, connected: JBooleanArray<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.restored";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let names = read_texts(env, &names, OP)?;
        let connected = read_bools(env, &connected)?;
        same_length(OP, &[peer_ids.len(), names.len(), connected.len()])?;
        Ok(RadioIngress::Restored {
            peers: peer_ids
                .into_iter()
                .zip(names)
                .zip(connected)
                .map(|((peer_id, name), connected)| RestoredPeer { peer_id, name, connected })
                .collect(),
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestDropped,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestDroppedForBinding,
    "mobile.ingest.dropped",
    (ingress_class: JString<'caller>, detail: JString<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.dropped";
        let class = match read_text(env, &ingress_class, OP)?.as_str() {
            "advertisement" => IngressClass::Advertisement,
            "notification" => IngressClass::Notification,
            "control" => IngressClass::Control,
            _ => return Err(invalid(OP, "class must be advertisement, notification or control")),
        };
        Ok(RadioIngress::Dropped {
            class,
            detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(),
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestDroppedCount,
    Java_com_ubm_core_MobileCoreBridge_nativeIngestDroppedCountForBinding,
    "mobile.ingest.dropped-count",
    (ingress_class: JString<'caller>, detail: JString<'caller>, count: jlong),
    |env| {
        const OP: &str = "mobile.ingest.dropped-count";
        let class = match read_text(env, &ingress_class, OP)?.as_str() {
            "advertisement" => IngressClass::Advertisement,
            "notification" => IngressClass::Notification,
            "control" => IngressClass::Control,
            _ => return Err(invalid(OP, "class must be advertisement, notification or control")),
        };
        let count = to_u64(count, OP)?;
        if count == 0 { return Err(invalid(OP, "count must be positive")); }
        Ok(RadioIngress::DroppedCount { class, detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(), count })
    }
);

#[cfg(ubm_jni_roundtrip)]
mod ordered_test_harness;

#[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
mod js_test_harness;

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_mobile::{Instance, ScanRequest};

    #[test]
    fn terminal_evidence_acknowledgement_window_is_bounded_and_lease_local() {
        let mut handles = TerminalEvidenceRecoveryHandles::default();
        for index in 0..=TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY {
            handles.retain_acknowledgement(format!("terminal-{index}"));
        }
        assert_eq!(
            handles.acknowledged.len(),
            TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY
        );
        assert!(!handles.acknowledged.contains("terminal-0"));
        assert!(handles.acknowledged.contains(&format!(
            "terminal-{TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY}"
        )));
        handles.clear_lease();
        assert!(handles.acknowledged.is_empty());
        assert!(handles.acknowledged_order.is_empty());
    }

    #[test]
    fn scan_refusal_wire_retains_ordered_and_nested_typed_evidence() {
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        use ubm_desktop::errors::{PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let mut platform = PlatformDetail::new("android", "6");
        platform.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "same".into(),
                value: PlatformValue::Int(i64::MIN),
            },
            PlatformMetadataEntry {
                key: "same".into(),
                value: PlatformValue::Bool(true),
            },
        ]);
        platform.cleanup_failures.push(
            DesktopError::new(BleErrorCode::BackendReset, BleErrorDomain::Core, "cleanup")
                .with_detail("nested"),
        );
        let error = DesktopError::new(
            BleErrorCode::ScanStartFailed,
            BleErrorDomain::Scan,
            "scan.start",
        )
        .with_detail("original")
        .with_platform(platform);
        let wire = scan_ingress_result(IngressStatus::ScanFailureRefused {
            error,
            closed: false,
        });
        assert_eq!(wire["status"], "refused");
        assert_eq!(wire["closed"], false);
        let tokens = wire["evidence"].as_array().unwrap();
        assert!(tokens.windows(6).any(|v| v
            == [
                serde_json::json!(["array", "2"]),
                serde_json::json!(["text", "same"]),
                serde_json::json!(["i64", i64::MIN.to_string()]),
                serde_json::json!(["array", "2"]),
                serde_json::json!(["text", "same"]),
                serde_json::json!(["bool", true]),
            ]));
        assert!(tokens
            .iter()
            .any(|v| *v == serde_json::json!(["text", "nested"])));
        assert_eq!(
            scan_ingress_result(IngressStatus::Accepted)["status"],
            "accepted"
        );
    }

    #[test]
    fn ordinary_request_ids_outside_positive_jlong_range_are_not_dispatched() {
        let owner =
            ubm_mobile::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        for id in [0, (i64::MAX as u64) + 1, u64::MAX] {
            let request = RadioRequest::AdapterState { id };
            assert!(
                validate_dispatch(&request, 2, owner).is_err(),
                "accepted {id}"
            );
        }
        for id in [1, i64::MAX as u64] {
            let request = RadioRequest::AdapterState { id };
            assert!(
                validate_dispatch(&request, 2, owner).is_ok(),
                "rejected {id}"
            );
        }
    }

    #[test]
    fn v2_slot_requests_keep_descriptor_and_attempt_id() {
        let identity = ubm_mobile::OrderedOpenIdentity {
            version: 2,
            owner_incarnation: ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
                2,
                "12345678-1234-4abc-8def-123456789abc",
            )
            .unwrap(),
            slot: 3,
            slot_generation: u64::MAX,
            original_request_id: 7,
            native_group_id: u64::MAX,
            peer_id: "peer".into(),
            spec: ubm_mobile::OrderedOpenSpec {
                connection_generation: "c".into(),
                database_generation: "d".into(),
                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "o".into(),
                    attachment: 4,
                    revision: 5,
                },
                members: vec![],
                item_capacity: 8,
                byte_capacity: 32,
            },
        };
        for (method, request) in [
            (
                "orderedOpenV2",
                ubm_mobile::OrderedOpenRequestV2::Open {
                    id: 7,
                    identity: identity.clone(),
                },
            ),
            (
                "orderedRecoverOpenV2",
                ubm_mobile::OrderedOpenRequestV2::Recover {
                    id: 8,
                    identity: identity.clone(),
                },
            ),
            (
                "orderedAcknowledgeOpenV2",
                ubm_mobile::OrderedOpenRequestV2::Acknowledge {
                    id: 9,
                    identity: identity.clone(),
                },
            ),
        ] {
            let (actual, args) = request_call(&RadioRequest::OrderedOpenV2(request));
            assert_eq!(actual, method);
            assert_eq!(
                signature(&args),
                "(JLcom/ubm/core/MobileCoreBridge$OrderedOpenIdentity;)V"
            );
            assert_eq!(args[1], Arg::OrderedOpenIdentity(identity.clone()));
        }
        let open = RadioRequest::OrderedOpenV2(ubm_mobile::OrderedOpenRequestV2::Open {
            id: 7,
            identity: identity.clone(),
        });
        assert!(validate_dispatch(&open, 2, identity.owner_incarnation).is_ok());
        assert!(validate_dispatch(&open, 1, identity.owner_incarnation).is_err());
        let overflow = RadioRequest::OrderedOpenV2(ubm_mobile::OrderedOpenRequestV2::Recover {
            id: u64::MAX,
            identity: identity.clone(),
        });
        assert!(validate_dispatch(&overflow, 2, identity.owner_incarnation).is_err());
        let stale_attempt =
            RadioRequest::OrderedOpenV2(ubm_mobile::OrderedOpenRequestV2::Acknowledge {
                id: 7,
                identity: identity.clone(),
            });
        assert!(validate_dispatch(&stale_attempt, 2, identity.owner_incarnation).is_err());
    }

    #[test]
    fn open_receipt_requests_preserve_exact_unsigned_identity() {
        for request in [
            RadioRequest::OrderedRecoverOpen {
                id: 5,
                group_id: u64::MAX,
                open_request_id: 1 << 63,
                peer_id: "peer".into(),
            },
            RadioRequest::OrderedAcknowledgeOpen {
                id: 5,
                version: 1,
                group_id: u64::MAX,
                open_request_id: 1 << 63,
                peer_id: "peer".into(),
            },
        ] {
            let (method, args) = request_call(&request);
            assert_eq!(
                method,
                if matches!(request, RadioRequest::OrderedRecoverOpen { .. }) {
                    "orderedRecoverOpen"
                } else {
                    "orderedAcknowledgeOpen"
                }
            );
            if matches!(request, RadioRequest::OrderedRecoverOpen { .. }) {
                assert_eq!(
                    args,
                    vec![id(5), epoch_arg(u64::MAX), epoch_arg(1 << 63), text("peer")]
                );
                assert_eq!(signature(&args), "(JJJLjava/lang/String;)V");
            } else {
                assert_eq!(
                    args,
                    vec![
                        id(5),
                        Arg::Int(1),
                        epoch_arg(u64::MAX),
                        epoch_arg(1 << 63),
                        text("peer")
                    ]
                );
                assert_eq!(signature(&args), "(JIJJLjava/lang/String;)V");
            }
        }
    }

    #[test]
    fn ordered_request_preserves_generation_and_unsigned_bits() {
        let (method, args) = request_call(&RadioRequest::OrderedConfirmCleanup {
            id: 42,
            generation: ubm_mobile::OrderedSourceGeneration::Counter(u64::MAX),
            group_id: u64::MAX,
            member: ubm_mobile::OrderedRegistration {
                member_id: u64::MAX,
                source_member: 1 << 63,
                epoch: u64::MAX,
            },
        });
        assert_eq!(method, "orderedRequest");
        assert_eq!(
            args,
            vec![
                id(42),
                text("confirm-cleanup"),
                text("counter"),
                text(&u64::MAX.to_string()),
                epoch_arg(u64::MAX),
                epoch_arg(u64::MAX),
                epoch_arg(1 << 63),
                epoch_arg(u64::MAX),
                epoch_arg(0)
            ]
        );
        assert_eq!(
            signature(&args),
            "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;JJJJJ)V"
        );
    }

    #[test]
    fn subrate_request_preserves_the_mode_and_native_request_id() {
        for mode in [
            ubm_mobile::SubrateMode::Default,
            ubm_mobile::SubrateMode::LowLatency,
            ubm_mobile::SubrateMode::LowPower,
            ubm_mobile::SubrateMode::HighThroughput,
        ] {
            let (method, args) = request_call(&RadioRequest::RequestSubrate {
                id: 42,
                peer_id: "peer".into(),
                mode,
            });
            assert_eq!(method, "requestSubrate");
            assert_eq!(
                args,
                vec![
                    Arg::Long(42),
                    Arg::Str(Some("peer".into())),
                    Arg::Str(Some(mode.as_str().into()))
                ]
            );
            assert_eq!(signature(&args), "(JLjava/lang/String;Ljava/lang/String;)V");
        }
    }

    fn hr() -> Instance {
        Instance {
            peer_id: "p".to_owned(),
            service_uuid: "s".to_owned(),
            service_occurrence: 0,
            characteristic_uuid: "c".to_owned(),
            characteristic_occurrence: 1,
        }
    }

    #[test]
    fn requests_map_to_typed_host_calls() {
        let (method, args) = request_call(&RadioRequest::EnableNotifications {
            id: 7,
            instance: hr(),
            epoch: 3,
            requested: Some(DeliveryMode::Indication),
            preferred: None,
        });
        assert_eq!(method, "enableNotifications");
        assert_eq!(
            signature(&args),
            "(JLjava/lang/String;Ljava/lang/String;JLjava/lang/String;JJLjava/lang/String;Ljava/lang/String;)V"
        );
        assert_eq!(args[7], Arg::Str(Some("indication".to_owned())));
        assert_eq!(args[8], Arg::Str(None));
        let (method, args) = request_call(&RadioRequest::StartScan {
            id: 1,
            scan: ScanRequest::default(),
        });
        assert_eq!(method, "startScan");
        assert_eq!(
            signature(&args),
            "(J[Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;IJLjava/lang/String;)V"
        );
        assert_eq!(args[5], Arg::Int(-1));
        assert_eq!(args[6], Arg::Long(0));
        assert_eq!(args[7], Arg::Str(None));
        let (_, args) = request_call(&RadioRequest::StartScan {
            id: 2,
            scan: ScanRequest {
                android: Some(ubm_mobile::AndroidScanOptions {
                    legacy: Some(false),
                    report_delay_ms: Some(500),
                    phy: Some(ubm_mobile::ScanPhy::LeCoded),
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        assert_eq!(args[5], Arg::Int(0));
        assert_eq!(args[6], Arg::Long(500));
        assert_eq!(args[7], Arg::Str(Some("coded".to_owned())));
        let (method, args) = request_call(&RadioRequest::Connect {
            id: 2,
            peer_id: "p".to_owned(),
            auto_connect: false,
            preferred_phy: vec![ubm_mobile::Phy::Le2m, ubm_mobile::Phy::LeCoded],
            expected_generation: "cg-1".to_owned(),
        });
        assert_eq!(method, "connect");
        assert_eq!(
            signature(&args),
            "(JLjava/lang/String;Z[Ljava/lang/String;Ljava/lang/String;)V"
        );
        assert_eq!(
            args[3],
            Arg::Strings(vec!["le-2m".to_owned(), "le-coded".to_owned()])
        );
        let (method, args) = request_call(&RadioRequest::ObservePresence {
            id: 11,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
        });
        assert_eq!(method, "observePresence");
        assert_eq!(signature(&args), "(JLjava/lang/String;)V");
        let (method, args) = request_call(&RadioRequest::StopPresence {
            id: 12,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
        });
        assert_eq!(method, "unobservePresence");
        assert_eq!(signature(&args), "(JLjava/lang/String;)V");
    }

    #[test]
    fn notification_epochs_preserve_all_u64_bits_on_both_request_paths() {
        for epoch in [0, i64::MAX as u64, 1_u64 << 63, u64::MAX] {
            let (_, enable) = request_call(&RadioRequest::EnableNotifications {
                id: 1,
                instance: hr(),
                epoch,
                requested: None,
                preferred: None,
            });
            let (method, disable) = request_call(&RadioRequest::DisableNotifications {
                id: 2,
                instance: hr(),
                epoch,
            });
            assert_eq!(method, "disableNotifications");
            assert_eq!(
                signature(&disable),
                "(JLjava/lang/String;Ljava/lang/String;JLjava/lang/String;JJ)V"
            );
            assert_eq!(enable[6], disable[6]);
            let Arg::Long(bits) = disable[6] else {
                panic!("epoch must cross as jlong")
            };
            assert_eq!(epoch_from_jlong(bits), epoch);
        }
    }

    #[test]
    fn every_request_signature_matches_the_java_interface() {
        let java =
            include_str!("../../../android/src/main/java/com/ubm/core/MobileCoreBridge.java");
        let requests = [
            RadioRequest::AdapterState { id: 1 },
            RadioRequest::StopScan { id: 1 },
            RadioRequest::Close { id: 1 },
            RadioRequest::BondedPeers { id: 1 },
            RadioRequest::Read {
                id: 1,
                instance: hr(),
            },
            RadioRequest::DisableNotifications {
                id: 1,
                instance: hr(),
                epoch: 1,
            },
            RadioRequest::Connect {
                id: 1,
                peer_id: "p".to_owned(),
                auto_connect: false,
                preferred_phy: Vec::new(),
                expected_generation: "cg-1".to_owned(),
            },
        ];
        for request in requests {
            let (method, _) = request_call(&request);
            assert!(
                java.contains(&format!("void {method}(long requestId")),
                "{method}"
            );
        }
    }

    #[test]
    fn discovery_tree_rebuilds_pre_order() {
        let tree = discovery_tree(
            &[0, 1, 2],
            &["180d".to_owned(), "2a37".to_owned(), "2902".to_owned()],
            &[0, 0, 0],
            &[0, 0x10, 0],
        );
        let tree = tree.unwrap_or_default();
        assert_eq!(tree.len(), 1);
        assert!(tree[0].characteristics[0].properties.notify);
        assert_eq!(tree[0].characteristics[0].descriptors.len(), 1);
        assert!(discovery_tree(&[1], &["x".to_owned()], &[0], &[0]).is_err());
        assert!(discovery_tree(&[0], &["x".to_owned()], &[-1], &[0]).is_err());
        let graph = discovery_tree(
            &[0, 3, 0, 0],
            &["a".into(), "a".into(), "a".into(), "b".into()],
            &[0, 1, 1, 0],
            &[5, 0, 6, 0],
        )
        .expect("service facts");
        assert_eq!(graph[0].primary, Some(true));
        assert_eq!(
            graph[0].included_services.as_ref().unwrap()[0].occurrence,
            1
        );
        assert_eq!(graph[1].primary, Some(false));
        assert_eq!(graph[1].included_services, Some(Vec::new()));
        assert_eq!(graph[2].primary, None);
        assert_eq!(graph[2].included_services, None);
        assert!(discovery_tree(&[0], &["x".into()], &[0], &[3]).is_err());
        assert!(discovery_tree(&[3], &["x".into()], &[0], &[0]).is_err());
    }

    #[test]
    fn jvm_smoke_compiles_the_single_shipped_java_authority() {
        let runner = include_str!("../run_mobile_roundtrip.sh");
        assert!(
            runner.contains("\"$ROOT/android/src/main/java/com/ubm/core/MobileCoreBridge.java\""),
            "JNI smoke must compile the shipped Android authority"
        );
        assert!(
            !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("java/com/ubm/core/MobileCoreBridge.java")
                .exists(),
            "a second manually maintained facade is forbidden"
        );
    }

    #[test]
    fn mobile_surface_has_no_staged_path() {
        let source = include_str!("mobile.rs");
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        for forbidden in [
            "ubm_fake_radio",
            "StagedDriver",
            "core_backend",
            "EchoBridge",
        ] {
            assert!(
                !body.contains(forbidden),
                "{forbidden} reachable from the mobile surface"
            );
        }
    }
}
#[test]
fn scan_recovery_owner_batch_capacity_is_all_or_nothing() {
    let mut handles = ScanRecoveryHandles {
        limit: 1,
        ..ScanRecoveryHandles::default()
    };
    let values = [
        PendingScanFailure {
            ordinal: 1,
            capability: [1; 16],
        },
        PendingScanFailure {
            ordinal: 2,
            capability: [2; 16],
        },
    ];
    assert!(handles.issue_owner_batch(7, "lease", &values).is_err());
    assert!(
        handles.owners.is_empty(),
        "capacity rejection must not mint a prefix"
    );
}
