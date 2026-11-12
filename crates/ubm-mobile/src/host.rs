//! [`MobileHost`]: the process-owned mobile radio owner.
//!
//! One platform radio and one [`DesktopCentral<ForeignRadio>`] per process,
//! on the shared executor; every RN manager is a [`MobileSession`] lease on
//! it (decision 4). One owner is what state restoration needs (the OS
//! hands restored peripherals to the process's one central, and the core
//! adopts them through the ordinary connect path), what several managers
//! sharing the platform radio need (one CoreBluetooth delegate, one
//! Android GATT owner), and what background operation needs (the owner
//! outlives any JS manager).
//!
//! Signals from the central (advertisements, values, lifecycle) and
//! host-level platform facts (adapter, security, restoration, scan
//! failure, ingress drops) enter one ordered queue; a single pump task
//! routes them to the sessions that hold the resource, so every session's
//! drain order matches the order the owner observed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use ubm_core::central::{Central, ConnectionState, PathSelector, canonical_uuid};
use ubm_core::contracts::{AttachmentTuple, BleErrorCode, BleErrorDomain, CoreError, OperationId};
use ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope;
use ubm_desktop::{
    Budget, CentralProfile, CentralSignal, DesktopCentral, DesktopError, InstanceKey,
    LifecycleEvent, LifecycleKind, NotificationPoll, OpControl, OpTicket, PeerRecord, PeerSnapshot,
    RadioEvent,
};

use crate::drain::Outbox;
use crate::foreign::{ForeignRadio, central_adapter_state, lock};
use crate::identity::MobileIdentity;
use crate::radio::{
    AdapterPower, AdapterSnapshot, Advertisement, AndroidScanOptions, BondState, IngressClass,
    IngressStatus, Instance, MobilePlatform, PlatformRadio, RadioCompletion, RadioIngress,
    RequestId, RequestKind, RestoredPeer, ScanRequest, SecurityState, WakeSink,
};
use crate::session::{MobileSession, SessionState};
use crate::wire::{self, object, opt_text};

#[path = "ordered_batch.rs"]
pub mod ordered_batch;
#[path = "ordered_wake.rs"]
pub mod ordered_wake;

/// Host identity at open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOptions {
    pub platform: MobilePlatform,
    /// The installing host's label (e.g. the app bundle id). Non-empty
    /// (`argument.invalid`, `ubm-mobile.host.owner`). It names no scope:
    /// the attachment identity is [`crate::MobileIdentity`]'s, in the legacy
    /// React Native formats, which carry no owner.
    pub owner: String,
    /// Adapter identity the platform reports (Android adapter address,
    /// `"corebluetooth"` on Apple). Non-empty.
    pub adapter_label: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SecurityEntry {
    pub state: SecurityState,
    pub owner_generation: Option<String>,
}

impl SecurityEntry {
    pub(crate) fn new(state: SecurityState) -> Self {
        Self {
            state,
            owner_generation: None,
        }
    }
}

fn reset_security_for_generation(
    entry: &mut SecurityEntry,
    generation: &str,
) -> Option<SecurityState> {
    if entry.owner_generation.as_deref() != Some(generation) {
        return None;
    }
    if entry.state.encryption != crate::radio::EncryptionState::Unsupported {
        entry.state.encryption = crate::radio::EncryptionState::Unknown;
    }
    if entry.state.authentication != crate::radio::AuthenticationState::Unsupported {
        entry.state.authentication = crate::radio::AuthenticationState::Unknown;
    }
    if entry.state.secure_connections != crate::radio::SecureConnectionsState::Unsupported {
        entry.state.secure_connections = crate::radio::SecureConnectionsState::Unknown;
    }
    entry.owner_generation = None;
    Some(entry.state.clone())
}

/// A fenced native owner is not one synthetic Rust failure.  The native
/// receipt contains the exact failures that still prevent retirement, and
/// callers need each of those identities to decide how to repair it.  Keep a
/// separate lifecycle record for the outstanding classes, then carry every
/// native failure through the normal public error mapper instead of rendering
/// the vector with `Debug` into one detail string.
fn pending_owner_retirement_cleanup(
    platform: MobilePlatform,
    outstanding_classes: &[crate::OwnerOutstandingClass],
    native_failures: &[crate::PlatformFailure],
) -> Vec<Value> {
    let state = DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Cleanup,
        "host.shutdown.ordered-owner",
    )
    .with_detail(format!(
        "native owner remains fenced pending {outstanding_classes:?}"
    ));
    let mut values = vec![crate::session::cleanup_failure("ordered-owner", &state)];
    values.extend(native_failures.iter().map(|failure| {
        crate::session::cleanup_failure(
            "ordered-owner",
            &failure.to_error(RequestKind::OwnerStatus, platform),
        )
    }));
    values
}

fn hex_identity(value: [u8; 16]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerSourceFailureRecoveryWire {
    t: String,
    v: u32,
    consumer: String,
    peer_id: String,
    selector: PeerSourceFailureSelectorWire,
    recipient_lease: String,
    descriptor: PeerSourceFailureDescriptorWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerSourceFailureSelectorWire {
    service_uuid: String,
    service_occurrence: Option<u64>,
    characteristic_uuid: Option<String>,
    characteristic_occurrence: Option<u64>,
    descriptor_uuid: Option<String>,
    descriptor_occurrence: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerSourceFailureDescriptorWire {
    protocol: u8,
    representation: String,
    host: String,
    peer: String,
    source_generation_identity: String,
    native_group: String,
    transfer: String,
    recovery: String,
    encoded_length: String,
    chunk_bytes: u32,
}

fn recovery_wire_error(operation: &'static str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::ArgumentInvalid,
        BleErrorDomain::Gatt,
        operation,
    )
    .with_detail("invalid peer source failure recovery record")
}

fn recovery_identity(value: &str, operation: &'static str) -> Result<[u8; 16], DesktopError> {
    if value.len() != 32
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value != value.to_ascii_lowercase()
    {
        return Err(recovery_wire_error(operation));
    }
    let mut identity = [0_u8; 16];
    for (index, byte) in identity.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| recovery_wire_error(operation))?;
    }
    Ok(identity)
}

pub(crate) fn parse_peer_source_failure_recovery(
    value: &str,
    operation: &'static str,
) -> Result<
    (
        ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        String,
        PathSelector,
        String,
        String,
    ),
    DesktopError,
> {
    if value.len() > wire::MAX_WIRE_TEXT_BYTES {
        return Err(recovery_wire_error(operation));
    }
    let wire: PeerSourceFailureRecoveryWire =
        serde_json::from_str(value).map_err(|_| recovery_wire_error(operation))?;
    if wire.t != "peer-source-failure-recovery"
        || wire.v != 2
        || wire.consumer.is_empty()
        || wire.peer_id.is_empty()
        || wire.recipient_lease.len() != 32
        || !wire
            .recipient_lease
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || wire.recipient_lease != wire.recipient_lease.to_ascii_lowercase()
    {
        return Err(recovery_wire_error(operation));
    }
    let selector = DesktopCentral::<ForeignRadio>::selector(
        &wire.selector.service_uuid,
        wire.selector.service_occurrence,
        wire.selector.characteristic_uuid.as_deref(),
        wire.selector.characteristic_occurrence,
        wire.selector.descriptor_uuid.as_deref(),
        wire.selector.descriptor_occurrence,
    )
    .map_err(|_| recovery_wire_error(operation))?;
    if wire.descriptor.protocol != ubm_core::oversized_evidence::PROTOCOL
        || wire.descriptor.representation
            != ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION
        || wire.descriptor.chunk_bytes == 0
    {
        return Err(recovery_wire_error(operation));
    }
    let encoded_length = wire
        .descriptor
        .encoded_length
        .parse::<u64>()
        .map_err(|_| recovery_wire_error(operation))?;
    if encoded_length.to_string() != wire.descriptor.encoded_length {
        return Err(recovery_wire_error(operation));
    }
    let descriptor = ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
        protocol: ubm_core::oversized_evidence::PROTOCOL,
        representation: ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION,
        identity: ubm_core::oversized_evidence::PeerSourceFailureIdentity {
            host: recovery_identity(&wire.descriptor.host, operation)?,
            peer: recovery_identity(&wire.descriptor.peer, operation)?,
            source_generation: recovery_identity(
                &wire.descriptor.source_generation_identity,
                operation,
            )?,
            native_group: recovery_identity(&wire.descriptor.native_group, operation)?,
            transfer: recovery_identity(&wire.descriptor.transfer, operation)?,
            recovery: recovery_identity(&wire.descriptor.recovery, operation)?,
        },
        encoded_length,
        chunk_bytes: wire.descriptor.chunk_bytes,
    };
    Ok((
        descriptor,
        wire.peer_id,
        selector,
        wire.consumer,
        wire.recipient_lease,
    ))
}

/// The recovery descriptor is an opaque capability, not a lossy error
/// summary. It is persisted raw; the continuation claim view is responsible
/// for admitting it only to an explicit V2 claimant.
fn peer_source_failure_recovery_record(
    consumer: &str,
    peer_id: &str,
    selector: &PathSelector,
    offer: ubm_desktop::central::PeerFailureRecoveryOffer,
) -> Value {
    // This route is not derived from opaque descriptor identities. The
    // recovery reader must present the exact live source-time path and
    // consumer to the central, which rejects a reconnect or a different
    // subscriber as stale instead of treating the descriptor as authority.
    let selector = serde_json::json!({
        "serviceUuid": selector.service_uuid,
        "serviceOccurrence": selector.service_occurrence,
        "characteristicUuid": selector.characteristic_uuid,
        "characteristicOccurrence": selector.characteristic_occurrence,
        "descriptorUuid": selector.descriptor_uuid,
        "descriptorOccurrence": selector.descriptor_occurrence,
    });
    serde_json::json!({
        "t":"peer-source-failure-recovery",
        "v":2,
        "consumer":consumer,
        "peerId":peer_id,
        "selector":selector,
        "recipientLease":offer.recipient_lease,
        "descriptor": {
            "protocol":offer.descriptor.protocol,
            "representation":offer.descriptor.representation,
            "host":hex_identity(offer.descriptor.identity.host),
            "peer":hex_identity(offer.descriptor.identity.peer),
            "sourceGenerationIdentity":hex_identity(offer.descriptor.identity.source_generation),
            "nativeGroup":hex_identity(offer.descriptor.identity.native_group),
            "transfer":hex_identity(offer.descriptor.identity.transfer),
            "recovery":hex_identity(offer.descriptor.identity.recovery),
            "encodedLength":offer.descriptor.encoded_length.to_string(),
            "chunkBytes":offer.descriptor.chunk_bytes,
        }
    })
}

#[cfg(test)]
mod peer_source_failure_recovery_tests {
    use super::{parse_peer_source_failure_recovery, peer_source_failure_recovery_record};
    use ubm_core::central::PathSelector;
    use ubm_core::oversized_evidence::{
        PEER_SOURCE_FAILURE_REPRESENTATION, PROTOCOL, PeerSourceFailureDescriptor,
        PeerSourceFailureIdentity,
    };
    use ubm_desktop::central::PeerFailureRecoveryOffer;

    #[test]
    fn descriptor_control_is_exactly_the_v2_cross_host_schema() {
        let descriptor = PeerSourceFailureDescriptor {
            protocol: PROTOCOL,
            representation: PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: PeerSourceFailureIdentity {
                host: [0x01; 16],
                peer: [0x02; 16],
                source_generation: [0x03; 16],
                native_group: [0x04; 16],
                transfer: [0x05; 16],
                recovery: [0x06; 16],
            },
            encoded_length: 123_456_789,
            chunk_bytes: 65_536,
        };
        let record = peer_source_failure_recovery_record(
            "heart-rate",
            "peer",
            &PathSelector {
                service_uuid: "180d".into(),
                service_occurrence: Some(0),
                characteristic_uuid: Some("2a37".into()),
                characteristic_occurrence: Some(0),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            },
            PeerFailureRecoveryOffer {
                descriptor,
                recipient_lease: "abababababababababababababababab".into(),
                error: None,
            },
        );
        assert_eq!(
            record,
            serde_json::json!({
                "t":"peer-source-failure-recovery", "v":2, "consumer":"heart-rate", "peerId":"peer",
                "selector":{"serviceUuid":"180d","serviceOccurrence":0,"characteristicUuid":"2a37","characteristicOccurrence":0,"descriptorUuid":null,"descriptorOccurrence":null},
                "recipientLease":"abababababababababababababababab",
                "descriptor": {
                    "protocol":1, "representation":"ubm-peer-source-failure/1",
                    "host":"01010101010101010101010101010101",
                    "peer":"02020202020202020202020202020202",
                    "sourceGenerationIdentity":"03030303030303030303030303030303",
                    "nativeGroup":"04040404040404040404040404040404",
                    "transfer":"05050505050505050505050505050505",
                    "recovery":"06060606060606060606060606060606",
                    "encodedLength":"123456789", "chunkBytes":65536,
                }
            })
        );
        let parsed = parse_peer_source_failure_recovery(
            &serde_json::to_string(&record).expect("record serializes"),
            "test",
        )
        .expect("route-bound recovery parses");
        assert_eq!(parsed.1, "peer");
        assert_eq!(parsed.3, "heart-rate");
        assert_eq!(parsed.4, "abababababababababababababababab");
    }
}

#[derive(Clone)]
pub(crate) struct RecordedRoute {
    pub recording_id: String,
    pub journal: Arc<ubm_desktop::continuation_journal::ContinuationJournal>,
    pub metadata: Value,
    pub order: Arc<tokio::sync::Mutex<()>>,
    pub radio_released: Arc<AtomicBool>,
}

impl std::fmt::Debug for RecordedRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordedRoute")
            .finish_non_exhaustive()
    }
}

/// One route from a characteristic instance to one session consumer.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub session_id: u64,
    pub consumer: String,
    pub core_consumer: String,
    pub peer_id: String,
    pub selector: PathSelector,
    pub delivery: &'static str,
    pub recorded: Option<RecordedRoute>,
    /// Caller-reserved native terminal slot, validated before CCCD activation.
    pub terminal_evidence_scope: Option<ScanTerminalEvidenceScope>,
    /// The terminal that ended this consumer's stream, kept until the
    /// session unsubscribes so `session.reconcile` can answer it after its
    /// `stream-end` record was lost.
    pub terminal: Option<StreamEnd>,
}

pub(crate) struct PendingRecordedRoute<'a> {
    host: &'a HostInner,
    session_id: u64,
    consumer: &'a str,
}
impl Drop for PendingRecordedRoute<'_> {
    fn drop(&mut self) {
        let routes = lock(&self.host.routes);
        let mut pending = lock(&self.host.pending_recordings);
        pending.remove(&(self.session_id, self.consumer.to_owned()));
        if !routes
            .values()
            .flatten()
            .any(|route| route.session_id == self.session_id && route.consumer == self.consumer)
            && let Some(session) = self.host.session(self.session_id)
        {
            session.outbox.remove_recording_hints(self.consumer);
        }
    }
}

/// The last link end the owner reported for one peer: what its `link`
/// record said, kept for `session.reconcile`.
#[derive(Debug, Clone)]
pub(crate) struct LinkEnd {
    pub connection_generation: String,
    pub database_generation: Option<String>,
    pub reason: &'static str,
}

/// One session's scan membership.
#[derive(Debug, Clone)]
pub(crate) struct ScanMember {
    pub membership: String,
    pub start_operation_id: String,
    pub service_uuids: Vec<String>,
    pub device_addresses: Vec<String>,
    /// Reservation scope held by the JS core until this scan's terminal is drained.
    pub terminal_evidence_scope: Option<ScanTerminalEvidenceScope>,
    pub deadline: Option<tokio::time::Instant>,
    /// Independent of the admission operation, which settles after start.
    pub expiry_cancel: OpTicket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanTerminalEvidenceScope {
    pub manager_id: String,
    pub operation_id: String,
    pub incarnation: String,
    /// Exact JS UTF-16 scope sidecar for exceptional unpaired surrogate
    /// identities. Scalar scopes retain the legacy `None` representation.
    pub scope_utf16le: Option<Vec<u8>>,
}

/// Receipt scope preserves the raw JavaScript UTF-16 identity when one was
/// required; scalar scopes keep the established three-string wire shape.
fn terminal_evidence_scope_value(scope: &ScanTerminalEvidenceScope) -> Value {
    let mut value = serde_json::json!({
        "managerId": scope.manager_id,
        "operationId": scope.operation_id,
        "incarnation": scope.incarnation,
    });
    if let Some(raw) = &scope.scope_utf16le {
        value["scopeUtf16leBase64"] = Value::String(wire::encode_base64(raw));
    }
    value
}

/// A terminal whose durable reservation has not committed. It remains wholly
/// native-owned: no drain acknowledgement is published until retry succeeds.
#[derive(Debug, Clone)]
struct ScanTerminalEvidenceDebt {
    session_id: u64,
    member: ScanMember,
    record: Value,
}

/// A notification terminal is retained verbatim until the trusted ledger has
/// staged and committed it. It is intentionally a route copy: later cleanup
/// cannot erase the producer identity needed for a retry.
#[derive(Debug, Clone)]
struct SubscriptionTerminalEvidenceDebt {
    route: Route,
    record: Value,
}

/// A terminal whose bytes committed while its session was still live. The
/// source keeps this ownership record until the session explicitly releases
/// the reservation or session teardown hands the committed bytes to recovery.
/// Keeping it independently of the session/outbox closes the race where
/// teardown follows a successful publication lookup but precedes delivery.
#[derive(Debug, Clone)]
struct PublishedSubscriptionTerminalEvidence {
    route: Route,
    record: Value,
    reservation_id: String,
}

#[derive(Debug)]
pub(crate) struct PhysicalScan {
    pub operation: OperationId,
    pub request: ScanRequest,
}

/// The shared physical scan and who is on it.
#[derive(Debug, Default)]
pub(crate) struct ScanShare {
    pub physical: Option<PhysicalScan>,
    orphan_retry_scheduled: bool,
}

/// Callback-time identity and recipients. This lock serializes scan admission
/// with capture; it is never held across a platform call or an await.
#[derive(Default)]
struct ScanIngress {
    failure_host_identity: Option<[u8; 16]>,
    generation: Option<RequestId>,
    attachment: Option<AttachmentTuple>,
    operation: Option<OperationId>,
    /// Set at callback ingress, before another member can join a failed source.
    failed_generation: Option<RequestId>,
    pending: Option<(u64, ScanMember)>,
    queue: VecDeque<ScanIngressItem>,
    /// Reservations and source copies owned by each admitted failure cut.
    failure_transfers: HashMap<usize, Vec<ScanFailureTransfer>>,
    failure_transfers_in_flight: usize,
    /// Callback-time records still owned by the pump, per recipient.
    pending_by_session: HashMap<u64, usize>,
    /// A disposed session remains reachable only by its captured scan work.
    retired_sessions: HashMap<u64, Arc<SessionState>>,
    /// Signals whose ordering depends on the in-flight start outcome. The
    /// failures retain their signal admission while deferred, including their
    /// associated cuts, even across successive starts.
    deferred_failures: VecDeque<HostSignal>,
    transfer_retry_scheduled: bool,
    deferred_reset: Option<HostSignal>,
    deferred_deadlines: bool,
}

struct ScanFailureTransfer {
    session: Arc<SessionState>,
    reservation: ubm_desktop::continuation_outbox::ScanFailureReservation,
    occurrence: Option<ubm_core::oversized_evidence::ScanFailureOccurrence>,
    captured: bool,
}

#[derive(Clone, Copy)]
struct ScanFailureSharedIdentity {
    host: [u8; 16],
    scan: [u8; 16],
    source_generation: [u8; 16],
    occurrence: [u8; 16],
}

impl ScanFailureTransfer {
    fn reserve(
        session: Arc<SessionState>,
        member: &ScanMember,
        generation: RequestId,
        source: &str,
        error: &DesktopError,
        shared: ScanFailureSharedIdentity,
    ) -> Option<Self> {
        use ubm_core::oversized_evidence::{
            EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureLimits,
            ScanFailureMember, ScanFailureOccurrence,
        };
        use ubm_desktop::continuation_outbox::SCAN_FAILURE_SOURCE_BYTES;

        let mut random = [0; 48];
        getrandom::fill(&mut random).ok()?;
        let id = |offset: usize| -> [u8; 16] {
            random[offset..offset + 16]
                .try_into()
                .expect("fixed identity length")
        };
        let identity = ScanFailureIdentity {
            host: shared.host,
            scan: shared.scan,
            start: id(0),
            source_generation: shared.source_generation,
            occurrence: shared.occurrence,
        };
        let recipient = ScanFailureMember {
            recipient: id(16),
            delivery: id(32),
        };
        let mut tokens = vec![EvidenceToken::Array(3)];
        tokens.extend(ubm_desktop::errors::DesktopErrorEvidence::from(error.clone()).into_tokens());
        tokens.push(EvidenceToken::Text(source.to_owned()));
        tokens.push(EvidenceToken::Array(3));
        tokens.push(EvidenceToken::U64(generation));
        tokens.push(EvidenceToken::Text(member.membership.clone()));
        tokens.push(EvidenceToken::Text(member.start_operation_id.clone()));
        let encoded = tokens.iter().try_fold(0u64, |total, token| {
            let size = match token {
                EvidenceToken::Null | EvidenceToken::Bool(_) => 1,
                EvidenceToken::Text(value) => 9u64.checked_add(value.len() as u64)?,
                EvidenceToken::Bytes(value) => 9u64.checked_add(value.len() as u64)?,
                _ => 9,
            };
            total.checked_add(size)
        })?;
        let chunks = encoded.div_ceil(u64::from(ubm_core::oversized_evidence::MAX_CHUNK_BYTES));
        let bitmap = usize::try_from(chunks.div_ceil(8)).ok()?;
        let bookkeeping = tokens
            .len()
            .checked_mul(16)?
            .checked_add(4096)?
            .checked_add(bitmap.checked_mul(2)?)?;
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: tokens.len().checked_add(1)?,
                max_tokens: tokens.len(),
                max_encoded_bytes: encoded,
                max_chunks: chunks,
                chunk_bytes: ubm_core::oversized_evidence::MAX_CHUNK_BYTES,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: bookkeeping,
        };
        let occurrence = ScanFailureOccurrence::new(
            identity,
            vec![recipient],
            Arc::new(EvidenceSource::new(tokens)),
        );
        let capacity = occurrence.retained_source_capacity(SCAN_FAILURE_SOURCE_BYTES)?;
        let reservation = session
            .outbox
            .reserve_scan_failure_with_source_capacity(limits, capacity)
            .ok()?;
        Some(Self {
            session,
            reservation,
            occurrence: Some(occurrence),
            captured: false,
        })
    }
}

/// A failure cut stays in the same bounded ingress queue as observations.
/// A pump turn must not cross it until its failure signal is processed.
enum ScanIngressItem {
    Advertisement(CapturedAdvertisement),
    FailureCut(Arc<()>),
}

struct CapturedAdvertisement {
    generation: RequestId,
    snapshot: PeerSnapshot,
    members: Vec<(u64, ScanMember)>,
    observed_at: u64,
}

/// A dropped admission future cannot leave the advertisement pump waiting
/// forever for a start result that will never publish its pending member.
struct ScanStartCapture<'a> {
    host: &'a HostInner,
    generation: RequestId,
}

impl Drop for ScanStartCapture<'_> {
    fn drop(&mut self) {
        let mut ingress = lock(&self.host.scan_ingress);
        let abandoned = ingress.generation == Some(self.generation) && ingress.pending.is_some();
        if abandoned {
            ingress.pending = None;
            ingress.generation = None;
        }
        let failures = std::mem::take(&mut ingress.deferred_failures);
        let reset = ingress.deferred_reset.take();
        let deadlines = std::mem::take(&mut ingress.deferred_deadlines);
        self.host.signals.resume_scan_signals(failures, reset);
        if deadlines {
            self.host.signals.push(HostSignal::ScanDeadlines);
        }
        drop(ingress);
        if abandoned {
            self.host.signals.push(HostSignal::Advertisements);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PeerInfo {
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub last_seen_ms: Option<u64>,
    pub source: &'static str,
}

/// Why a consumer's stream ended: (reason, dropped items, dropped bytes).
pub(crate) type StreamEnd = (&'static str, u64, u64, Option<DesktopError>);

/// Result of draining one notification route for a bounded pump turn.
/// `committed` means at least one polled group reached the session's outbox
/// (admitted or refused), which is the cost a scope turn must not repeat.
enum RouteDrain {
    /// The route is still installed. `pending` means the budget stopped the
    /// drain while the core still holds values.
    Live { committed: bool, pending: bool },
    /// The route reached a terminal answer.
    Ended {
        committed: bool,
        terminal: StreamEnd,
        loss_accounted: bool,
    },
}

impl RouteDrain {
    fn committed(&self) -> bool {
        match self {
            Self::Live { committed, .. } | Self::Ended { committed, .. } => *committed,
        }
    }
}

/// What ended one poll run of a value group, applied only after the values
/// polled before it are committed.
#[derive(Clone)]
enum PollEnd {
    /// The group is full; the core may still hold more.
    Full,
    Empty,
    Ended(StreamEnd),
}

/// The terminal a refused value group ends its route with. Every polled value
/// the group could not admit is counted once, together with the core's own
/// terminal loss when it answered in the same poll run. `None` means a journal
/// cutoff refused the whole remaining group: those values are counted in the
/// handoff cutoff, not in a fabricated stream terminal.
fn refusal_terminal(
    rejected: &ubm_desktop::continuation_outbox::DataBatchRejection,
    core: Option<StreamEnd>,
) -> Option<StreamEnd> {
    use ubm_desktop::continuation_outbox::DataIngressFailure as Failure;
    let (core_items, core_bytes) = core
        .as_ref()
        .map_or((0, 0), |(_, items, bytes, _)| (*items, *bytes));
    match rejected.failure {
        // A full queue remains a genuine overflow and includes this group's
        // refused values in its terminal loss.
        Failure::Overflow { .. } => Some((
            "overflow",
            rejected.items.saturating_add(core_items),
            rejected.bytes.saturating_add(core_bytes),
            None,
        )),
        // The session's journal failure retains the precise storage cause;
        // closed is the frozen wire lifecycle name, never a fabricated queue
        // overflow.
        Failure::Storage { .. } => Some((
            "closed",
            rejected.items.saturating_add(core_items),
            rejected.bytes.saturating_add(core_bytes),
            None,
        )),
        // Stopped and sealed journal cutoffs have already counted every
        // rejected record after the cutoff. Do not turn later records into a
        // fabricated overflow; preserve only a genuine core terminal.
        Failure::Stopped { .. } | Failure::Sealed { .. } => core,
    }
}

enum HostSignal {
    Advertisements,
    /// At least one value scope is dirty; the scopes themselves wait in the
    /// dirty set, so any number of them costs this one queue slot.
    Values,
    Lifecycle(LifecycleEvent),
    /// A platform adapter change, with the attachment it happened under.
    Adapter(AdapterSnapshot, u64, AttachmentTuple),
    /// The core reset the attachment after an adapter loss: the new
    /// attachment, and whether the reset ended the owned scan.
    AdapterReset(
        AttachmentTuple,
        Option<OperationId>,
        ubm_desktop::AdapterLossCause,
        Vec<(Arc<SessionState>, crate::session::ResetResources)>,
    ),
    #[cfg(test)]
    ScanFailed(String),
    ScanFailedScoped(
        DesktopError,
        RequestId,
        Vec<(u64, ScanMember)>,
        Arc<()>,
        &'static str,
    ),
    ScanDeadlines,
    Security(String, SecurityState),
    SecurityFailed(Option<String>, DesktopError),
    /// Latest write-without-response readiness for one peer. The generation
    /// is the connection generation current when the report arrived.
    WriteReadiness(String, Option<String>, bool),
    Restored(Vec<RestoredPeer>),
    IngressDrop(IngressClass, u64),
}

/// Bound for queued host signals (X-R6). Value scopes share one queued
/// marker, the advertisement signal one, and every other current-state fact
/// merges per scope below, so a stalled pump plus a burst retains a bounded
/// prefix: at most `SIGNALS_CAP + 6` queue entries whatever the number of scopes
/// (value, advertisement, deadline and adapter-reset markers, plus two scan terminals).
/// Each admitted scan failure remains charged while in flight or deferred,
/// and owns exactly one cut in the separately bounded advertisement ingress.
/// What the bound refuses is counted in `signal_lost` / `overflow_drops`
/// and broadcast by the pump — never silently discarded.
const SIGNALS_CAP: usize = 1024;

/// Dirty value scopes one marker handling flushes. Bounded so one marker
/// cannot starve lifecycle and current-state signals; leftovers requeue
/// the marker for another turn.
const VALUE_SCOPE_BATCH: usize = 32;

/// Journaled records one scope turn may commit, which is also the most one
/// synced SQLite commit carries. The pump groups only values the core already
/// holds, never waiting to fill a group, so a commit costs one sync chain
/// instead of one per record. A scope turn commits at most one such group
/// across all of its routes, then yields: an unbounded drain, or one group per
/// route, would hold every already-queued security and lifecycle signal until
/// the whole backlog had been written.
const VALUE_RECORD_BATCH: usize = ubm_desktop::continuation_journal::APPEND_BATCH_MAX;

/// One advertisement turn must yield to queued lifecycle/deadline markers.
const ADVERTISEMENT_BATCH: usize = 32;

#[cfg(test)]
async fn pump_advertisement_batch<T, F: std::future::Future<Output = Option<T>>>(
    mut take: impl FnMut() -> F,
    mut deliver: impl FnMut(T),
) -> bool {
    for _ in 0..ADVERTISEMENT_BATCH {
        let Some(record) = take().await else {
            return false;
        };
        deliver(record);
    }
    // Do not probe one extra record: that would consume an undelivered
    // observation. An empty follow-up turn is safe and constantly bounded.
    true
}

const INGRESS_CLASSES: [IngressClass; 3] = [
    IngressClass::Advertisement,
    IngressClass::Notification,
    IngressClass::Control,
];

const fn ingress_index(class: IngressClass) -> usize {
    match class {
        IngressClass::Advertisement => 0,
        IngressClass::Notification => 1,
        IngressClass::Control => 2,
    }
}

#[derive(Default)]
struct SignalState {
    queue: VecDeque<HostSignal>,
    scan_failures_outstanding: usize,
    scan_failure_reserved: bool,
    scan_failure_emergency: bool,
    source_failure_refused: Option<(&'static str, &'static str, Option<String>)>,
    /// Dirty value scopes in round-robin order. The queue holds at most
    /// one marker for all of them, so the queue — not this list — is the
    /// bounded channel. `dirty_members` keeps `push_value` idempotent.
    dirty: VecDeque<InstanceKey>,
    dirty_members: HashSet<InstanceKey>,
    /// A `Values` marker already waits in the queue.
    value_marker_queued: bool,
    advertisements_pending: bool,
    scan_deadlines_pending: bool,
    closed: bool,
    /// Non-coalescible signals refused past the bound (lifecycle
    /// transitions, and current-state facts with no queued marker to merge
    /// into). The pump broadcasts one control ingress-drop per session for
    /// these, which drives `session.reconcile` from retained owner truth.
    signal_lost: u64,
    /// Ingress-drop counts that arrived while the queue was full, per
    /// class. The pump broadcasts them class-accurately to every session.
    overflow_drops: [u64; 3],
}

/// Ordered signal queue between the central (and ingress) and the pump.
/// Value scopes share one queued marker and advertisement signals one: the
/// pump polls the core's bounded queues, so one pending marker is enough
/// and the queue stays constantly bounded whatever the number of scopes.
/// Current-state facts (adapter, security, restored set, scan outcome,
/// reset, ingress-drop counts) merge per scope too: only the latest is
/// ever queued. Lifecycle transitions never coalesce — past the bound
/// they are counted and reconciled.
#[derive(Default)]
struct Signals {
    state: Mutex<SignalState>,
    notify: Notify,
}

#[derive(Debug, PartialEq, Eq)]
enum ScanFailureAdmission {
    Queued,
    Emergency,
    Closed,
}

impl Signals {
    // A failure owns its admission until its terminal and cut have both
    // retired, including while popped or deferred outside this queue.
    fn occupied(state: &SignalState) -> usize {
        let queued_failures = state
            .queue
            .iter()
            .filter(|signal| matches!(signal, HostSignal::ScanFailedScoped(..)))
            .count();
        state.queue.len() + state.scan_failures_outstanding - queued_failures
    }

    fn finish_scan_failure(&self) {
        let mut state = lock(&self.state);
        state.scan_failures_outstanding -= 1;
        if Self::occupied(&state) < SIGNALS_CAP {
            state.scan_failure_reserved = false;
        }
    }

    fn resume_scan_signals(&self, mut failures: VecDeque<HostSignal>, reset: Option<HostSignal>) {
        if failures.is_empty() && reset.is_none() {
            return;
        }
        let mut state = lock(&self.state);
        if let Some(reset) = reset {
            state.queue.push_front(reset);
        }
        while let Some(failure) = failures.pop_back() {
            state.queue.push_front(failure);
        }
        drop(state);
        self.notify.notify_one();
    }
    /// Return an already admitted failure behind earlier work without charging
    /// admission again or allowing ordinary queue pressure to discard it.
    fn requeue_scan_failure(&self, signal: HostSignal) {
        lock(&self.state).queue.push_back(signal);
        self.notify.notify_one();
    }

    /// Queue one signal, unless it merges into a queued one. A closed queue
    /// refuses everything (host teardown); a full queue refuses only what
    /// cannot merge, counting it for the pump's overflow broadcast.
    fn fail_source_ingress(
        &self,
        domain: &'static str,
        operation: &'static str,
        detail: Option<String>,
    ) {
        let mut state = lock(&self.state);
        if state.source_failure_refused.is_none() {
            state.source_failure_refused = Some((domain, operation, detail));
        }
        drop(state);
        self.notify.notify_one();
    }

    fn take_source_ingress_failure(&self) -> Option<(&'static str, &'static str, Option<String>)> {
        lock(&self.state).source_failure_refused.take()
    }

    fn try_push_scan_failure(
        &self,
        error: DesktopError,
        generation: RequestId,
        members: Vec<(u64, ScanMember)>,
        cut: Arc<()>,
        source: &'static str,
    ) -> Result<ScanFailureAdmission, DesktopError> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(error);
        }
        let admission = if Self::occupied(&state) >= SIGNALS_CAP {
            if state.scan_failure_reserved {
                if state.scan_failure_emergency {
                    return Err(error);
                }
                state.scan_failure_emergency = true;
                ScanFailureAdmission::Emergency
            } else {
                state.scan_failure_reserved = true;
                ScanFailureAdmission::Queued
            }
        } else {
            ScanFailureAdmission::Queued
        };
        state.scan_failures_outstanding += 1;
        state.queue.push_back(HostSignal::ScanFailedScoped(
            error, generation, members, cut, source,
        ));
        drop(state);
        self.notify.notify_one();
        Ok(admission)
    }

    #[cfg(test)]
    fn push_scan_failure(
        &self,
        error: DesktopError,
        generation: RequestId,
        members: Vec<(u64, ScanMember)>,
        cut: Arc<()>,
        source: &'static str,
    ) -> ScanFailureAdmission {
        self.try_push_scan_failure(error, generation, members, cut, source)
            .unwrap_or(ScanFailureAdmission::Closed)
    }

    fn push(&self, signal: HostSignal) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            match signal {
                HostSignal::Values => Self::ensure_value_marker(&mut state),
                HostSignal::Advertisements => {
                    if state.advertisements_pending {
                        return;
                    }
                    state.advertisements_pending = true;
                    state.queue.push_back(HostSignal::Advertisements);
                }
                HostSignal::ScanDeadlines => {
                    if state.scan_deadlines_pending {
                        return;
                    }
                    // One reserved marker drives all expired memberships. It
                    // cannot be dropped behind ordinary control queue pressure.
                    state.scan_deadlines_pending = true;
                    state.queue.push_back(HostSignal::ScanDeadlines);
                }
                HostSignal::Adapter(snapshot, updated_at, attachment) => {
                    let signal = HostSignal::Adapter(snapshot, updated_at, attachment);
                    if let Some(slot) = state
                        .queue
                        .iter_mut()
                        .find(|queued| matches!(queued, HostSignal::Adapter(..)))
                    {
                        // The latest platform snapshot wins: an adapter
                        // record carries current state, not an event.
                        *slot = signal;
                    } else {
                        Self::push_bounded(&mut state, signal);
                    }
                }
                HostSignal::AdapterReset(attachment, ended_scan, cause, mut resources) => {
                    // Empty snapshots carry no ownership evidence and otherwise
                    // retain one session reference for every coalesced reset.
                    resources.retain(|(_, captured)| !captured.is_empty());
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::AdapterReset(current, ended, current_cause, captured) =
                            queued
                        {
                            captured.append(&mut resources);
                            *current = attachment.clone();
                            if ended_scan.is_some() {
                                *current_cause = cause;
                                *ended = ended_scan.clone();
                            }
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        // Reset can wait behind a scan failure cut; reserve
                        // its coalesced marker so retry cannot lose teardown.
                        state.queue.push_back(HostSignal::AdapterReset(
                            attachment, ended_scan, cause, resources,
                        ));
                    }
                }
                #[cfg(test)]
                HostSignal::ScanFailed(detail) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::ScanFailed(current) = queued {
                            *current = detail.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::ScanFailed(detail));
                    }
                }
                HostSignal::ScanFailedScoped(..) => {
                    unreachable!("scan failures require reserved admission");
                }
                HostSignal::Security(peer_id, observed) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::Security(existing, current) = queued
                            && *existing == peer_id
                        {
                            *current = observed.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::Security(peer_id, observed));
                    }
                }
                HostSignal::WriteReadiness(peer_id, generation, ready) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::WriteReadiness(
                            existing,
                            current_generation,
                            current_ready,
                        ) = queued
                            && *existing == peer_id
                        {
                            *current_generation = generation.clone();
                            *current_ready = ready;
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(
                            &mut state,
                            HostSignal::WriteReadiness(peer_id, generation, ready),
                        );
                    }
                }
                HostSignal::Restored(peers) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::Restored(current) = queued {
                            *current = peers.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::Restored(peers));
                    }
                }
                HostSignal::IngressDrop(class, count) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::IngressDrop(existing, total) = queued
                            && *existing == class
                        {
                            *total += count;
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_countable(&mut state, class, count);
                    }
                }
                HostSignal::SecurityFailed(peer, error) => {
                    Self::push_bounded(&mut state, HostSignal::SecurityFailed(peer, error));
                }
                HostSignal::Lifecycle(event) => {
                    if Self::occupied(&state) >= SIGNALS_CAP {
                        state.signal_lost += 1;
                    } else {
                        state.queue.push_back(HostSignal::Lifecycle(event));
                    }
                }
            }
        }
        self.notify.notify_one();
    }

    /// Queue a current-state fact, or count it when the queue is full. A
    /// counted fact is re-readable: the pump's overflow broadcast drives
    /// `session.reconcile`, which answers every such fact from retained
    /// owner truth (adapter snapshot, security table, restored set, scan
    /// memberships).
    fn push_bounded(state: &mut SignalState, signal: HostSignal) {
        if Self::occupied(state) >= SIGNALS_CAP {
            state.signal_lost += 1;
        } else {
            state.queue.push_back(signal);
        }
    }

    /// Queue an ingress-drop marker, merging per class. A marker that finds
    /// no room keeps its count in the per-class overflow tally, which the
    /// pump broadcasts class-accurately.
    fn push_countable(state: &mut SignalState, class: IngressClass, count: u64) {
        if Self::occupied(state) >= SIGNALS_CAP {
            state.overflow_drops[ingress_index(class)] += count;
        } else {
            state.queue.push_back(HostSignal::IngressDrop(class, count));
        }
    }

    /// Queue one dirty value scope. The scope joins the dirty set; at most
    /// one marker waits in the queue for all of them, so any number of
    /// scopes costs one queue slot. The marker may pass the cap by one
    /// slot: dropping it would strand dirty scopes with no marker, and one
    /// slot keeps the queue's constant bound.
    fn push_value(&self, scope: InstanceKey) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            if state.dirty_members.insert(scope.clone()) {
                state.dirty.push_back(scope);
            }
            Self::ensure_value_marker(&mut state);
        }
        self.notify.notify_one();
    }

    /// Queue the value marker unless one already waits. Callers hold the
    /// signal lock.
    fn ensure_value_marker(state: &mut SignalState) {
        if !state.value_marker_queued {
            state.value_marker_queued = true;
            state.queue.push_back(HostSignal::Values);
        }
    }

    /// Take up to `max` dirty value scopes from the front of the FIFO.
    /// A caller that still has values pushes that scope back, behind the
    /// scopes that have not had this cycle's turn. Values within a scope
    /// stay ordered by the core queue the pump polls.
    fn take_value_batch(&self, max: usize) -> Vec<InstanceKey> {
        let mut state = lock(&self.state);
        let mut batch = Vec::with_capacity(max.min(state.dirty.len()));
        while batch.len() < max {
            let Some(scope) = state.dirty.pop_front() else {
                break;
            };
            state.dirty_members.remove(&scope);
            batch.push(scope);
        }
        batch
    }

    /// Requeue the value marker while dirty scopes remain (the consumed
    /// marker drained one bounded batch). May pass the cap by one slot,
    /// like the initial queue, so no scope is ever stranded.
    fn requeue_values_if_dirty(&self) {
        let queued = {
            let mut state = lock(&self.state);
            if state.dirty.is_empty() || state.value_marker_queued {
                false
            } else {
                state.value_marker_queued = true;
                state.queue.push_back(HostSignal::Values);
                true
            }
        };
        if queued {
            self.notify.notify_one();
        }
    }

    /// Take accumulated overflow for the pump to broadcast. Counts reset:
    /// every lost signal is reported exactly once, to every session.
    fn take_overflow(&self) -> (u64, [u64; 3]) {
        let mut state = lock(&self.state);
        (
            std::mem::take(&mut state.signal_lost),
            std::mem::take(&mut state.overflow_drops),
        )
    }

    #[cfg(test)]
    pub(crate) fn queue_len(&self) -> usize {
        lock(&self.state).queue.len()
    }

    #[cfg(test)]
    pub(crate) fn dirty_len(&self) -> usize {
        lock(&self.state).dirty.len()
    }

    fn pop(&self) -> Option<HostSignal> {
        let mut state = lock(&self.state);
        let signal = state.queue.pop_front()?;
        if Self::occupied(&state) < SIGNALS_CAP {
            state.scan_failure_reserved = false;
        }
        match &signal {
            HostSignal::Values => state.value_marker_queued = false,
            HostSignal::Advertisements => state.advertisements_pending = false,
            HostSignal::ScanDeadlines => state.scan_deadlines_pending = false,
            _ => {}
        }
        Some(signal)
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.notify.notify_one();
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }
}

/// Who a background (foreground-service) lease belongs to (87/N8).
///
/// A shared scope is one React Native module instance: its leases outlive
/// the manager that acquired them (legacy held them on the native module
/// until `invalidate()`), any session of that scope may update or release
/// them, and [`MobileHost::release_background_scope`] ends them. A session
/// opened without a scope is its own scope, so `session.dispose` ends it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BackgroundScope {
    Session(u64),
    Shared(String),
}

pub(crate) struct HostInner {
    pub platform: MobilePlatform,
    pub runtime: Handle,
    pub radio: ForeignRadio,
    ordered_admission_owner: Option<crate::radio::OrderedAdmissionOwner>,
    ordered_retirement_owner: Option<crate::radio::OrderedAdmissionOwner>,
    ordered_group_owner: Option<(
        Arc<crate::foreign::MobileOpenCoordinator>,
        crate::ordered_open_slots::HostIdentity,
    )>,
    pub central: DesktopCentral<ForeignRadio>,
    pub wake: Arc<dyn WakeSink>,
    signals: Arc<Signals>,
    pub ordered_wakes: Arc<ordered_wake::OrderedWakeRegistry>,
    pub(crate) ordered_owners: crate::ordered_owners::OrderedOwners,
    pub(crate) ordered_start_receipts:
        Mutex<HashMap<(u64, u64), crate::session::RecordingStartReceipt>>,
    pub(crate) ordered_recording_start_guard: tokio::sync::Mutex<()>,
    pub(crate) ordered_recordings: Mutex<
        HashMap<(u64, String), Arc<ubm_desktop::ordered_recording::OrderedRecording<ForeignRadio>>>,
    >,
    ordered_consumers: Mutex<
        HashMap<
            ubm_desktop::ordered_notification_groups::SourceKey,
            Arc<ordered_batch::OrderedBatchConsumer>,
        >,
    >,
    ordered_pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    ordered_recovery_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    ordered_recovery_stop: Arc<Notify>,
    pub sessions: Mutex<BTreeMap<u64, Arc<SessionState>>>,
    /// The host-local session handle for each opaque recording attached for
    /// scan recovery. Persisted session ids are source proof, not process-wide
    /// identities: separate recordings are both allowed to have started at 1.
    recovered_scan_recordings: Mutex<BTreeMap<String, u64>>,
    next_session: AtomicU64,
    #[cfg(test)]
    before_session_admission: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    after_scan_start: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
    /// Consumers `drain_route` polled, in order. Debug tests read it.
    /// Release builds omit it.
    #[cfg(debug_assertions)]
    route_turns: Mutex<Vec<String>>,
    /// The consumers whose group each value scope turn committed, in order.
    /// Debug tests read it. Release builds omit it.
    #[cfg(debug_assertions)]
    scope_turns: Mutex<Vec<Vec<String>>>,
    pub scan: tokio::sync::Mutex<ScanShare>,
    pub scan_members: Mutex<BTreeMap<u64, ScanMember>>,
    scan_terminal_evidence_debts: Mutex<Vec<ScanTerminalEvidenceDebt>>,
    subscription_terminal_evidence_debts: Mutex<Vec<SubscriptionTerminalEvidenceDebt>>,
    subscription_terminal_evidence_publications:
        Mutex<HashMap<String, PublishedSubscriptionTerminalEvidence>>,
    #[cfg(any(test, feature = "test-support"))]
    scan_terminal_evidence_commit_failures_remaining: AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    subscription_terminal_evidence_commit_failures_remaining: AtomicUsize,
    #[cfg(test)]
    subscription_terminal_evidence_after_commit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Process-owned key that authenticates an exact scan cleanup retry after
    /// its session and outbox have been reclaimed. It never crosses a host
    /// boundary or enters a recording.
    scan_finalization_secret: [u8; 32],
    scan_ingress: Mutex<ScanIngress>,
    pub routes: Mutex<HashMap<InstanceKey, Vec<Route>>>,
    pending_recordings: Mutex<HashMap<(u64, String), RecordedRoute>>,
    recorded_drops_settled: AtomicU64,
    recorded_drops_notify: Notify,
    pub directory: Mutex<BTreeMap<String, PeerInfo>>,
    pub restored: Mutex<BTreeMap<String, RestoredPeer>>,
    /// Restored peers already handed to an adopter, and the session that
    /// claimed each. Claims last for the process (legacy consumed the OS
    /// restoration identifiers once per process): a later manager never
    /// adopts the same peer again. Lock after `restored`.
    pub restoration_claims: Mutex<BTreeMap<String, u64>>,
    pub security: Mutex<HashMap<String, SecurityEntry>>,
    pub security_failures: Mutex<BTreeMap<Option<String>, (u64, DesktopError)>>,
    pub security_failure_revision: AtomicU64,
    pub adapter: Mutex<Option<(AdapterSnapshot, u64)>>,
    /// Live background leases per scope. A lease leaves only when the
    /// platform confirmed its release; a failed release stays for a retry.
    pub background: Mutex<BTreeMap<BackgroundScope, BTreeSet<String>>>,
    /// The latest link end per peer (one entry per peer).
    pub link_ends: Mutex<BTreeMap<String, LinkEnd>>,
    /// The latest database change per peer: (connection generation, the
    /// database generation the change invalidated).
    pub database_changes: Mutex<BTreeMap<String, (String, String)>>,
    shut_down: AtomicBool,
    // Session publication is fenced before disposal; scan ingress remains
    // independently open until its accepted transfers and receipts settle.
    session_admission_closed: AtomicBool,
    shutdown_state: tokio::sync::Mutex<HostShutdownState>,
    shutdown_owner_phase: tokio::sync::watch::Sender<bool>,
    shutdown_observed_failures: Mutex<Vec<Value>>,
    shutdown_running: Mutex<bool>,
    shutdown_updates: tokio::sync::watch::Sender<Option<String>>,
    shutdown_completion: Mutex<ShutdownCompletion>,
    ingress_failure: Mutex<Option<Value>>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    clock: Instant,
    clock_id: String,
    pub(crate) continuation: std::sync::OnceLock<ubm_desktop::continuation::NativeContinuation>,
    pub(crate) recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    /// Installed by the trusted process bootstrap.  A normal manager may
    /// create this process host before a later manager elects durable terminal
    /// evidence, so this is an attachable authority rather than an open-time
    /// snapshot.
    pub(crate) terminal_evidence:
        Mutex<Option<Arc<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger>>>,
    /// Host-configured stable application identity used only to select its
    /// own opaque retained terminal evidence after a process restart.
    pub(crate) terminal_evidence_recipient: String,
    /// The same trusted durable recovery owner used by desktop hosts. Mobile
    /// keeps its session/ingress accounting around the lease, while foreign
    /// hosts can use the registry without pretending to be a MobileHost.
    pub(crate) scan_failure_recovery_registry:
        Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry>,
    pub(crate) continuation_closed: AtomicBool,
    pub(crate) continuation_admission: Mutex<()>,
}

// Keep an attempt's accumulated failures reachable if a later cleanup
// callback unwinds before the normal receipt wrapper can publish them.
struct ShutdownFailures<'a> {
    values: Vec<Value>,
    observed: &'a Mutex<Vec<Value>>,
}

impl std::ops::Deref for ShutdownFailures<'_> {
    type Target = Vec<Value>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl std::ops::DerefMut for ShutdownFailures<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

impl Drop for ShutdownFailures<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let mut observed = lock(self.observed);
            for failure in &self.values {
                if !observed.contains(failure) {
                    observed.push(failure.clone());
                }
            }
        }
    }
}

#[derive(Default)]
struct ShutdownCompletion {
    observer: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    receipt: Option<String>,
}

#[derive(Default)]
struct HostShutdownState {
    central_released: bool,
    recovery_stopped: bool,
    local_cleanup_settled: bool,
    retirement_started: bool,
    retirement_retryable: bool,
    worker_failures: Vec<Value>,
    complete: Option<String>,
}

/// The process-owned mobile owner. Cloning shares it.
#[derive(Clone)]
pub struct MobileHost {
    pub(crate) inner: Arc<HostInner>,
}

/// A session-scoped recovery handle. Constructed only after authenticating the
/// session authority; it retains the outbox across concurrent disposal.
pub struct ScanFailureRecovery {
    host: Arc<HostInner>,
    state: Arc<SessionState>,
}

/// Trusted-host lease over terminal evidence selected by the installing
/// process identity.  It intentionally has no API accepting a manager,
/// recording, reservation or evidence reference from a foreign caller.
#[derive(Clone)]
pub struct TerminalEvidenceRecovery {
    host: Arc<HostInner>,
    lease: Arc<TerminalEvidenceRecoveryLease>,
}

/// Private recovery-lease state shared by cloned native recovery handles.
/// Dropping the final handle releases only its runtime record authorities; it
/// never deletes the retained terminal rows.
struct TerminalEvidenceRecoveryLease {
    id: String,
    ledger: Arc<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger>,
    /// Serializes recovery admission with close so an operation cannot claim
    /// rows after its lease has been explicitly released.
    admission: Mutex<()>,
    closed: AtomicBool,
    #[cfg(test)]
    before_list_claim: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    selected:
        Mutex<HashMap<String, ubm_desktop::terminal_evidence_ledger::RecoverableTerminalEvidence>>,
    acknowledged: Mutex<(VecDeque<String>, HashSet<String>)>,
}

impl Drop for TerminalEvidenceRecoveryLease {
    fn drop(&mut self) {
        // Drop cannot report a poisoned in-process mutex. Normal bindings use
        // the explicit close below; this remains a best-effort OS-lock guard
        // so leaked controller handles cannot pin durable rows forever.
        let _ = self.ledger.release_recovery_lease(&self.id);
    }
}

/// Public metadata for one opaque native recovery entry.  `reservation_id`
/// remains private and is only consumed by the trusted lease on acknowledge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalEvidenceRecoveryEntry {
    reservation_id: String,
    pub producer: String,
    pub scope: TerminalEvidenceScope,
    pub state: TerminalEvidenceRecoveryState,
    pub reference: Option<String>,
    pub byte_length: Option<u64>,
    pub digest: Option<[u8; 32]>,
    pub format_version: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalEvidenceRecoveryState {
    Staged,
    Committed,
}

/// A bounded view of terminal evidence selected for native recovery.
///
/// `more` is true whenever a retained row could not be returned within the
/// requested item or byte bound. Callers acknowledge returned rows before
/// asking for the next page; no foreign cursor is accepted by this authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalEvidenceRecoveryListPage {
    pub entries: Vec<TerminalEvidenceRecoveryEntry>,
    pub more: bool,
}

/// Additive recovery listing result that reports a concurrently owned
/// producer without hiding independently recoverable records.  The legacy
/// [`TerminalEvidenceRecovery::list`] method deliberately retains its strict
/// busy behavior for wire compatibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalEvidenceRecoveryStatusPage {
    pub entries: Vec<TerminalEvidenceRecoveryEntry>,
    pub more: bool,
    pub busy_producers: Vec<String>,
}

fn terminal_recovery_error(
    failure: ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError,
) -> DesktopError {
    use ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError;
    match failure {
        TerminalEvidenceError::Quota => DesktopError::new(
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
            "terminal-evidence-recovery",
        ),
        TerminalEvidenceError::Busy => DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "terminal-evidence-recovery",
        ),
        TerminalEvidenceError::Missing
        | TerminalEvidenceError::Unresolved
        | TerminalEvidenceError::NotStaged => DesktopError::new(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Stream,
            "terminal-evidence-recovery",
        ),
        TerminalEvidenceError::Invalid(_) | TerminalEvidenceError::Conflict => DesktopError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Stream,
            "terminal-evidence-recovery",
        ),
        TerminalEvidenceError::Corrupt | TerminalEvidenceError::Storage(_) => DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "terminal-evidence-recovery",
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalEvidenceRecoveryPage {
    pub offset: u64,
    pub next_offset: u64,
    pub bytes: Vec<u8>,
    pub done: bool,
}

/// An authenticated confirmation, safe to retain and replay after reclamation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureCleanupProof {
    pub receipt: ubm_desktop::continuation_outbox::ScanFailureReleaseConfirmation,
    /// Stable authority-bound MAC for a live or durable rehydrated outbox.
    pub authenticator: [u8; 32],
    /// Process-bound MAC used only after the owning session/outbox was
    /// actually reclaimed. It prevents a caller from fabricating an
    /// authority/proof pair for a missing owner.
    pub reclamation_authenticator: [u8; 32],
}

type ScanOutboxError = ubm_desktop::continuation_outbox::ScanFailureOutboxError;
type ScanHead = ubm_desktop::continuation_outbox::ScanFailureHead;
type ScanPending = ubm_desktop::continuation_outbox::PendingScanFailure;

fn scan_recovery_error() -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Core,
        "scan.failure-recovery",
    )
    .with_detail("unknown session or invalid recovery authority")
}

fn scan_recovery_unsupported() -> DesktopError {
    DesktopError::new(
        BleErrorCode::CapabilityUnsupported,
        BleErrorDomain::Core,
        "scan.failure-recovery.attach",
    )
    .with_detail("recording has no authenticated v2 scan recovery authority")
}

impl HostInner {
    pub(crate) fn terminal_evidence_ledger(
        &self,
    ) -> Option<Arc<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger>> {
        let existing = { lock(&self.terminal_evidence).clone() };
        if let Some(ledger) = existing {
            return Some(ledger);
        }
        // Configuration and host publication happen through separate native
        // boundary locks. If configuration lands in their narrow interleave,
        // converge here before admitting a reservation or recovery lease.
        // Attachment is idempotent and never mutates a shutting-down host.
        let maximum = crate::continuation::terminal_evidence_maximum_bytes()?;
        self.attach_terminal_evidence(maximum).ok()?;
        lock(&self.terminal_evidence).clone()
    }

    fn attach_terminal_evidence(&self, maximum_bytes: u64) -> Result<(), DesktopError> {
        if self.is_shut_down() || self.continuation_closed.load(Ordering::SeqCst) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "ubm-mobile.host.terminal-evidence",
            ));
        }
        let ledger = Arc::new(
            self.recording_registry
                .terminal_evidence_ledger(maximum_bytes)
                .map_err(|error| {
                    DesktopError::new(
                        BleErrorCode::CapabilityUnavailable,
                        BleErrorDomain::Core,
                        "ubm-mobile.host.terminal-evidence",
                    )
                    .with_detail(error.to_string())
                })?,
        );
        let mut installed = lock(&self.terminal_evidence);
        if installed.is_none() {
            *installed = Some(ledger);
        }
        Ok(())
    }

    /// Commits the exact terminal payload before it is exposed to the drain.
    /// The receipt is deliberately constructed only after durable commit, so a
    /// consumer can never observe an acknowledgement for bytes that were not
    /// accepted by the ledger.
    fn commit_scan_terminal_evidence(
        &self,
        member: &ScanMember,
        record: &Value,
    ) -> Result<Option<Value>, ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError> {
        let Some(scope) = &member.terminal_evidence_scope else {
            return Ok(None);
        };
        let ledger = self
            .terminal_evidence_ledger()
            .ok_or(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
        let reservation = ledger.attach(&TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: scope.manager_id.clone(),
            operation_id: scope.operation_id.clone(),
            incarnation: scope.incarnation.clone(),
            scope_utf16le: scope.scope_utf16le.clone(),
        })?;
        let canonical = serde_json::to_vec(record).map_err(|_| {
            ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Invalid("scan terminal")
        })?;
        #[cfg(any(test, feature = "test-support"))]
        if self
            .scan_terminal_evidence_commit_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Busy);
        }
        let committed = ledger.commit(
            &reservation.reservation_id,
            &canonical,
            Sha256::digest(&canonical).into(),
        )?;
        Ok(Some(serde_json::json!({
            "producer":"scan", "kind":"committed", "ref":committed.reference,
            "byteLength":committed.byte_length, "digest":committed.digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "formatVersion":committed.format_version, "durability":"committed",
            "scope":terminal_evidence_scope_value(scope)
        })))
    }

    /// Verify that a scan's caller-reserved durable terminal slot exists
    /// before the radio is activated. `attach` is deliberately allocation
    /// free, so this cannot turn an untrusted scope into a new reservation.
    pub(crate) fn verify_scan_terminal_evidence_scope(
        &self,
        scope: Option<&ScanTerminalEvidenceScope>,
    ) -> Result<(), ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError> {
        let Some(scope) = scope else { return Ok(()) };
        let ledger = self
            .terminal_evidence_ledger()
            .ok_or(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
        ledger.attach(&TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: scope.manager_id.clone(),
            operation_id: scope.operation_id.clone(),
            incarnation: scope.incarnation.clone(),
            scope_utf16le: scope.scope_utf16le.clone(),
        })?;
        Ok(())
    }

    /// Subscription activation has the same precondition as scan activation,
    /// but its producer namespace is deliberately distinct so a scan slot
    /// cannot authorize a notification terminal.
    pub(crate) fn verify_subscription_terminal_evidence_scope(
        &self,
        scope: Option<&ScanTerminalEvidenceScope>,
    ) -> Result<(), ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError> {
        let Some(scope) = scope else { return Ok(()) };
        let ledger = self
            .terminal_evidence_ledger()
            .ok_or(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
        ledger.attach(&TerminalEvidenceScope {
            producer: "subscription".to_owned(),
            manager_id: scope.manager_id.clone(),
            operation_id: scope.operation_id.clone(),
            incarnation: scope.incarnation.clone(),
            scope_utf16le: scope.scope_utf16le.clone(),
        })?;
        Ok(())
    }

    fn has_scan_terminal_evidence_debt(&self, session_id: u64) -> bool {
        lock(&self.scan_terminal_evidence_debts)
            .iter()
            .any(|debt| debt.session_id == session_id)
    }

    fn publish_scan_terminal_or_retain(
        &self,
        session_id: u64,
        member: ScanMember,
        mut record: Value,
    ) {
        if let Some(scope) = &member.terminal_evidence_scope {
            let stage = (|| {
                let recipient = self.terminal_evidence_recipient.clone();
                let ledger = self.terminal_evidence_ledger().ok_or(
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved,
                )?;
                let reservation = ledger.attach(&TerminalEvidenceScope {
                    producer: "scan".to_owned(),
                    manager_id: scope.manager_id.clone(),
                    operation_id: scope.operation_id.clone(),
                    incarnation: scope.incarnation.clone(),
                    scope_utf16le: scope.scope_utf16le.clone(),
                })?;
                let canonical = serde_json::to_vec(&record).map_err(|_| {
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Invalid(
                        "scan terminal",
                    )
                })?;
                ledger.stage(&reservation.reservation_id, &recipient, &canonical)
            })();
            if stage.is_err() {
                lock(&self.scan_terminal_evidence_debts).push(ScanTerminalEvidenceDebt {
                    session_id,
                    member,
                    record,
                });
                return;
            }
        }
        match self.commit_scan_terminal_evidence(&member, &record) {
            Ok(Some(evidence)) => {
                record
                    .as_object_mut()
                    .expect("terminal record object")
                    .insert("evidence".to_owned(), evidence);
            }
            Ok(None) => {}
            Err(_) => {
                lock(&self.scan_terminal_evidence_debts).push(ScanTerminalEvidenceDebt {
                    session_id,
                    member,
                    record,
                });
                return;
            }
        }
        if let Some(session) = self.scan_session(session_id) {
            session.outbox.push_control_before_seal(record);
        }
        self.forget_retired_scan_session_if_settled(session_id);
    }

    /// Explicit recovery boundary: retry exact retained terminal bytes and
    /// publish only after the ledger acknowledges the commit.
    pub(crate) fn retry_scan_terminal_evidence_debts(&self) {
        let debts = std::mem::take(&mut *lock(&self.scan_terminal_evidence_debts));
        for debt in debts {
            self.publish_scan_terminal_or_retain(debt.session_id, debt.member, debt.record);
        }
    }

    /// A shutting-down source cannot retain an OS source authority for bytes
    /// that were already staged. Hand that exact reservation to restart
    /// recovery; an unsuccessful handoff remains an explicit shutdown debt.
    fn handoff_scan_terminal_evidence_debts_to_recovery(&self) -> Vec<DesktopError> {
        let debts = std::mem::take(&mut *lock(&self.scan_terminal_evidence_debts));
        let mut retained = Vec::new();
        let mut failures = Vec::new();
        for debt in debts {
            let Some(scope) = debt.member.terminal_evidence_scope.as_ref() else {
                continue;
            };
            let outcome = (|| {
                let ledger = self.terminal_evidence_ledger().ok_or(
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved,
                )?;
                let reservation = ledger.attach(&TerminalEvidenceScope {
                    producer: "scan".to_owned(),
                    manager_id: scope.manager_id.clone(),
                    operation_id: scope.operation_id.clone(),
                    incarnation: scope.incarnation.clone(),
                    scope_utf16le: scope.scope_utf16le.clone(),
                })?;
                ledger.handoff_staged_to_recovery(&reservation.reservation_id)
            })();
            if let Err(error) = outcome {
                failures.push(terminal_recovery_error(error));
                retained.push(debt);
            }
        }
        if !retained.is_empty() {
            lock(&self.scan_terminal_evidence_debts).extend(retained);
        }
        failures
    }

    fn forget_retired_scan_session_if_settled(&self, session_id: u64) {
        let mut ingress = lock(&self.scan_ingress);
        if !ingress.pending_by_session.contains_key(&session_id)
            && !self.has_scan_terminal_evidence_debt(session_id)
            && ingress
                .retired_sessions
                .get(&session_id)
                .is_some_and(|session| session.outbox.pending_scan_failure_owners() == 0)
        {
            ingress.retired_sessions.remove(&session_id);
        }
    }
    fn allocate_session_id(
        &self,
        sessions: &BTreeMap<u64, Arc<SessionState>>,
    ) -> Result<u64, DesktopError> {
        loop {
            let id = self
                .next_session
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| {
                    DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Core,
                        "ubm-mobile.session.identity",
                    )
                    .with_detail("host session identity namespace exhausted")
                })?;
            if !sessions.contains_key(&id) {
                return Ok(id);
            }
        }
    }

    fn authenticate_scan_authority(
        &self,
        state: &SessionState,
        authority: &[u8; 32],
    ) -> Result<(), DesktopError> {
        let expected = state
            .outbox
            .scan_failure_recovery_authority()
            .map_err(|_| scan_recovery_error())?
            .ok_or_else(scan_recovery_error)?;
        let challenge = b"ubm-mobile-scan-authority-compare/1";
        let mut supplied = Hmac::<Sha256>::new_from_slice(authority).expect("HMAC key length");
        supplied.update(challenge);
        let supplied = supplied.finalize().into_bytes();
        let mut expected_mac = Hmac::<Sha256>::new_from_slice(&expected).expect("HMAC key length");
        expected_mac.update(challenge);
        expected_mac
            .verify_slice(&supplied)
            .map_err(|_| scan_recovery_error())
    }

    fn scan_cleanup_mac(
        &self,
        id: u64,
        authority: &[u8; 32],
        receipt: &ubm_desktop::continuation_outbox::ScanFailureReleaseConfirmation,
    ) -> Result<Hmac<Sha256>, DesktopError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(authority).expect("HMAC key length");
        mac.update(b"ubm-mobile-scan-cleanup/1");
        mac.update(&id.to_be_bytes());
        mac.update(&receipt.ordinal.to_be_bytes());
        mac.update(&receipt.capability);
        mac.update(&[u8::from(receipt.raw)]);
        mac.update(&receipt.authenticator);
        Ok(mac)
    }

    fn reclaimed_scan_cleanup_mac(
        &self,
        id: u64,
        authority: &[u8; 32],
        receipt: &ubm_desktop::continuation_outbox::ScanFailureReleaseConfirmation,
    ) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.scan_finalization_secret)
            .expect("HMAC key length");
        mac.update(b"ubm-mobile-scan-cleanup-reclaimed/1");
        mac.update(&id.to_be_bytes());
        mac.update(authority);
        mac.update(&receipt.ordinal.to_be_bytes());
        mac.update(&receipt.capability);
        mac.update(&[u8::from(receipt.raw)]);
        mac.update(&receipt.authenticator);
        mac
    }
}

impl ScanFailureRecovery {
    pub fn owners(&self) -> Vec<ubm_desktop::continuation_outbox::ScanFailureRecoveryOwner> {
        self.state.outbox.scan_failure_recovery_owners()
    }
    pub fn member(
        &self,
        key: &ScanPending,
        index: usize,
    ) -> Result<ubm_core::oversized_evidence::ScanFailureMember, ScanOutboxError> {
        self.state.outbox.scan_failure_recovery_member(key, index)
    }
    pub fn head(
        &self,
        member: ubm_core::oversized_evidence::ScanFailureMember,
    ) -> Result<Option<ScanHead>, ScanOutboxError> {
        self.state.outbox.scan_failure_head(member)
    }
    pub fn read(
        &self,
        head: &ScanHead,
        offset: u64,
    ) -> Result<ubm_core::oversized_evidence::EvidenceChunk, ScanOutboxError> {
        self.state.outbox.read_scan_failure(head, offset)
    }
    pub fn acknowledge(&self, head: &ScanHead) -> Result<(), ScanOutboxError> {
        self.state.outbox.acknowledge_scan_failure(head)
    }
    pub fn retire(&self, head: &ScanHead) -> Result<(), ScanOutboxError> {
        self.state.outbox.retire_scan_failure(head)
    }
    pub fn release(&self, head: &ScanHead) -> Result<(), ScanOutboxError> {
        self.state.outbox.release_scan_failure_receipt(head)
    }
    fn proof(
        &self,
        receipt: ubm_desktop::continuation_outbox::ScanFailureReleaseConfirmation,
    ) -> Result<ScanFailureCleanupProof, DesktopError> {
        let authority = self
            .state
            .outbox
            .scan_failure_recovery_authority()
            .map_err(|_| scan_recovery_error())?
            .ok_or_else(scan_recovery_error)?;
        let authenticator = self
            .host
            .scan_cleanup_mac(self.state.id, &authority, &receipt)?
            .finalize()
            .into_bytes()
            .into();
        let reclamation_authenticator = self
            .host
            .reclaimed_scan_cleanup_mac(self.state.id, &authority, &receipt)
            .finalize()
            .into_bytes()
            .into();
        Ok(ScanFailureCleanupProof {
            receipt,
            authenticator,
            reclamation_authenticator,
        })
    }
    pub fn confirm_release(
        &self,
        head: &ScanHead,
    ) -> Result<ScanFailureCleanupProof, DesktopError> {
        let receipt = self
            .state
            .outbox
            .confirm_scan_failure_receipt_release(head)
            .map_err(|_| scan_recovery_error())?;
        self.proof(receipt)
    }

    /// Finalize a confirmation without ever returning either the durable
    /// recovery authority or the authenticated cleanup proof to a foreign
    /// caller. The recovery handle itself is already bound to this exact
    /// session/outbox by the trusted host attachment path.
    pub fn finalize_release(&self, proof: &ScanFailureCleanupProof) -> Result<(), DesktopError> {
        let authority = self
            .state
            .outbox
            .scan_failure_recovery_authority()
            .map_err(|_| scan_recovery_error())?
            .ok_or_else(scan_recovery_error)?;
        self.host
            .scan_cleanup_mac(self.state.id, &authority, &proof.receipt)?
            .verify_slice(&proof.authenticator)
            .map_err(|_| scan_recovery_error())?;
        let mut ingress = lock(&self.host.scan_ingress);
        self.state
            .outbox
            .finalize_scan_failure_release(&proof.receipt)
            .map_err(|_| scan_recovery_error())?;
        if !ingress.pending_by_session.contains_key(&self.state.id)
            && self.state.outbox.pending_scan_failure_owners() == 0
        {
            ingress.retired_sessions.remove(&self.state.id);
        }
        Ok(())
    }

    /// Finalize a released prepared descriptor and return the opaque durable
    /// cleanup receipt.  Native foreign bindings retain this value only as a
    /// handle; the receipt's capability never crosses their serialization
    /// boundary.  The caller must explicitly retire it after persisting its
    /// own outcome.
    pub fn finalize_release_with_retirement(
        &self,
        head: &ScanHead,
    ) -> Result<ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement, DesktopError> {
        let receipt = self
            .state
            .outbox
            .confirm_scan_failure_receipt_release(head)
            .map_err(|_| scan_recovery_error())?;
        self.state
            .outbox
            .finalize_scan_failure_release_with_retirement(&receipt)
            .map_err(|_| scan_recovery_error())
    }

    /// Reacquire durable cleanup receipts following a cold host reopen.  No
    /// receipt bytes are returned by this Rust boundary.
    pub fn pending_cleanup_retirements(
        &self,
    ) -> Result<Vec<ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement>, DesktopError>
    {
        self.state
            .outbox
            .scan_failure_cleanup_retirements()
            .map_err(|_| scan_recovery_error())
    }

    /// Explicit, idempotent retirement of one opaque cleanup receipt.
    pub fn retire_cleanup_retirement(
        &self,
        receipt: &ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement,
    ) -> Result<(), DesktopError> {
        self.state
            .outbox
            .retire_scan_failure_cleanup(receipt)
            .map_err(|_| scan_recovery_error())
    }
    pub fn inspect_raw(
        &self,
        key: &ScanPending,
    ) -> Result<ubm_desktop::continuation_outbox::ScanFailureRawInfo, ScanOutboxError> {
        self.state.outbox.inspect_scan_failure(key)
    }
    pub fn inspect_raw_member(
        &self,
        key: &ScanPending,
        index: usize,
    ) -> Result<ubm_core::oversized_evidence::ScanFailureMember, ScanOutboxError> {
        self.state.outbox.inspect_scan_failure_member(key, index)
    }
    pub fn read_raw_token(
        &self,
        key: &ScanPending,
        index: usize,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ubm_desktop::continuation_outbox::ScanFailureRawToken, ScanOutboxError> {
        self.state
            .outbox
            .inspect_scan_failure_token(key, index, offset, max_bytes)
    }
    pub fn retire_raw(&self, key: &ScanPending) -> Result<(), ScanOutboxError> {
        self.state.outbox.retire_unprepared_scan_failure(key)
    }
    pub fn release_raw(&self, key: &ScanPending) -> Result<(), ScanOutboxError> {
        self.state
            .outbox
            .release_unprepared_scan_failure_receipt(key)
    }
    pub fn confirm_raw_release(
        &self,
        key: &ScanPending,
    ) -> Result<ScanFailureCleanupProof, DesktopError> {
        let receipt = self
            .state
            .outbox
            .confirm_unprepared_scan_failure_release(key)
            .map_err(|_| scan_recovery_error())?;
        self.proof(receipt)
    }

    /// Raw-source counterpart to [`Self::finalize_release_with_retirement`].
    pub fn finalize_raw_release_with_retirement(
        &self,
        key: &ScanPending,
    ) -> Result<ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement, DesktopError> {
        let receipt = self
            .state
            .outbox
            .confirm_unprepared_scan_failure_release(key)
            .map_err(|_| scan_recovery_error())?;
        self.state
            .outbox
            .finalize_scan_failure_release_with_retirement(&receipt)
            .map_err(|_| scan_recovery_error())
    }
}

impl TerminalEvidenceRecovery {
    /// Explicitly release this native recovery lease's runtime authorities.
    /// Retained records stay durable and can be selected by a later lease.
    /// Bindings should call this during controlled teardown; [`Drop`] is only
    /// the best-effort fallback for abandoned handles.
    pub fn close(&self) -> Result<(), DesktopError> {
        let _admission = lock(&self.lease.admission);
        if self
            .lease
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        if let Err(error) = self
            .lease
            .ledger
            .release_recovery_lease(&self.lease.id)
            .map_err(terminal_recovery_error)
        {
            self.lease.closed.store(false, Ordering::Release);
            return Err(error);
        }
        lock(&self.lease.selected).clear();
        let mut acknowledged = lock(&self.lease.acknowledged);
        acknowledged.0.clear();
        acknowledged.1.clear();
        Ok(())
    }

    fn ledger(
        &self,
        operation: &'static str,
    ) -> Result<Arc<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger>, DesktopError>
    {
        if self.lease.closed.load(Ordering::Acquire) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                operation,
            ));
        }
        if self.host.is_shut_down() {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                operation,
            ));
        }
        self.host.terminal_evidence_ledger().ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Core,
                operation,
            )
            .with_detail("trusted terminal evidence storage was not configured")
        })
    }

    /// Enumerate only rows staged for this process's persisted native owner.
    /// Bounds are checked before any retained bytes are cloned.
    pub fn list(
        &self,
        maximum_items: u32,
        maximum_bytes: u64,
    ) -> Result<TerminalEvidenceRecoveryListPage, DesktopError> {
        const OP: &str = "terminal-evidence-recovery.list";
        if maximum_items == 0 || maximum_bytes == 0 {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                OP,
            ));
        }
        let _admission = lock(&self.lease.admission);
        let ledger = self.ledger(OP)?;
        #[cfg(test)]
        if let Some(hook) = lock(&self.lease.before_list_claim).as_ref() {
            hook();
        }
        let scan = ledger
            .recoverable_page_for_lease(
                "scan",
                &self.host.terminal_evidence_recipient,
                &self.lease.id,
                maximum_items,
                maximum_bytes,
            )
            .map_err(terminal_recovery_error)?;
        let used_bytes = scan.entries.iter().try_fold(0_u64, |total, value| {
            total
                .checked_add(value.canonical_bytes.len() as u64)
                .ok_or_else(|| {
                    DesktopError::new(BleErrorCode::ArgumentInvalid, BleErrorDomain::Stream, OP)
                })
        })?;
        let remaining_items =
            maximum_items.saturating_sub(u32::try_from(scan.entries.len()).unwrap_or(u32::MAX));
        let (values, more) = if scan.more {
            (scan.entries, true)
        } else if remaining_items == 0 || used_bytes >= maximum_bytes {
            let subscription_more = ledger
                .has_recoverable("subscription", &self.host.terminal_evidence_recipient)
                .map_err(terminal_recovery_error)?;
            (scan.entries, subscription_more)
        } else {
            let subscription = ledger
                .recoverable_page_for_lease(
                    "subscription",
                    &self.host.terminal_evidence_recipient,
                    &self.lease.id,
                    remaining_items,
                    maximum_bytes - used_bytes,
                )
                .map_err(terminal_recovery_error)?;
            let mut values = scan.entries;
            values.extend(subscription.entries);
            (values, subscription.more)
        };
        let mut total = 0_u64;
        let mut entries = Vec::new();
        let mut selected = lock(&self.lease.selected);
        for value in values {
            if entries.len() >= maximum_items as usize {
                return Ok(TerminalEvidenceRecoveryListPage {
                    entries,
                    more: true,
                });
            }
            let next_total = total
                .checked_add(value.canonical_bytes.len() as u64)
                .ok_or_else(|| {
                    DesktopError::new(BleErrorCode::ArgumentInvalid, BleErrorDomain::Stream, OP)
                })?;
            if next_total > maximum_bytes {
                return Ok(TerminalEvidenceRecoveryListPage {
                    entries,
                    more: true,
                });
            }
            total = next_total;
            selected.insert(value.reservation_id.clone(), value.clone());
            entries.push(TerminalEvidenceRecoveryEntry {
            reservation_id: value.reservation_id,
            producer: value.scope.producer.clone(),
            scope: value.scope,
            state: match value.state {
                ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Staged => TerminalEvidenceRecoveryState::Staged,
                ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Committed => TerminalEvidenceRecoveryState::Committed,
            },
            reference: value.reference,
            byte_length: value.byte_length,
            digest: value.digest,
            format_version: value.format_version,
            });
        }
        Ok(TerminalEvidenceRecoveryListPage { entries, more })
    }

    /// Enumerate cold retained records while reporting producers with a
    /// source or another recovery lease still holding authority.  This is an
    /// additive route: callers that require the historical all-or-nothing
    /// behavior continue to use [`Self::list`].
    pub fn list_with_status(
        &self,
        maximum_items: u32,
        maximum_bytes: u64,
    ) -> Result<TerminalEvidenceRecoveryStatusPage, DesktopError> {
        const OP: &str = "terminal-evidence-recovery.list-status";
        if maximum_items == 0 || maximum_bytes == 0 {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                OP,
            ));
        }
        let _admission = lock(&self.lease.admission);
        let ledger = self.ledger(OP)?;
        let scan = ledger
            .recoverable_page_for_lease_with_contention(
                "scan",
                &self.host.terminal_evidence_recipient,
                &self.lease.id,
                maximum_items,
                maximum_bytes,
            )
            .map_err(terminal_recovery_error)?;
        let used_bytes = scan.entries.iter().try_fold(0_u64, |total, value| {
            total
                .checked_add(value.canonical_bytes.len() as u64)
                .ok_or_else(|| {
                    DesktopError::new(BleErrorCode::ArgumentInvalid, BleErrorDomain::Stream, OP)
                })
        })?;
        let remaining_items =
            maximum_items.saturating_sub(u32::try_from(scan.entries.len()).unwrap_or(u32::MAX));
        let mut busy_producers = Vec::new();
        if scan.busy {
            busy_producers.push("scan".to_owned());
        }
        let (values, more) = if scan.more {
            (scan.entries, true)
        } else if remaining_items == 0 || used_bytes >= maximum_bytes {
            let subscription_more = ledger
                .has_recoverable("subscription", &self.host.terminal_evidence_recipient)
                .map_err(terminal_recovery_error)?;
            (scan.entries, subscription_more)
        } else {
            let subscription = ledger
                .recoverable_page_for_lease_with_contention(
                    "subscription",
                    &self.host.terminal_evidence_recipient,
                    &self.lease.id,
                    remaining_items,
                    maximum_bytes - used_bytes,
                )
                .map_err(terminal_recovery_error)?;
            if subscription.busy {
                busy_producers.push("subscription".to_owned());
            }
            let mut values = scan.entries;
            values.extend(subscription.entries);
            (values, subscription.more)
        };
        let mut total = 0_u64;
        let mut entries = Vec::new();
        let mut selected = lock(&self.lease.selected);
        for value in values {
            let next_total = total
                .checked_add(value.canonical_bytes.len() as u64)
                .ok_or_else(|| {
                    DesktopError::new(BleErrorCode::ArgumentInvalid, BleErrorDomain::Stream, OP)
                })?;
            if entries.len() >= maximum_items as usize || next_total > maximum_bytes {
                return Ok(TerminalEvidenceRecoveryStatusPage {
                    entries,
                    more: true,
                    busy_producers,
                });
            }
            total = next_total;
            selected.insert(value.reservation_id.clone(), value.clone());
            entries.push(TerminalEvidenceRecoveryEntry {
                reservation_id: value.reservation_id,
                producer: value.scope.producer.clone(),
                scope: value.scope,
                state: match value.state {
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Staged => TerminalEvidenceRecoveryState::Staged,
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Committed => TerminalEvidenceRecoveryState::Committed,
                },
                reference: value.reference,
                byte_length: value.byte_length,
                digest: value.digest,
                format_version: value.format_version,
            });
        }
        Ok(TerminalEvidenceRecoveryStatusPage {
            entries,
            more,
            busy_producers,
        })
    }

    /// Return one bounded window of the exact staged bytes.  The entry must
    /// originate from this lease's list result; a forged reservation never
    /// reaches the ledger because bindings retain the entry behind an opaque
    /// handle.
    pub fn read(
        &self,
        entry: &TerminalEvidenceRecoveryEntry,
        offset: u64,
        maximum_bytes: u32,
    ) -> Result<TerminalEvidenceRecoveryPage, DesktopError> {
        const OP: &str = "terminal-evidence-recovery.read";
        let _admission = lock(&self.lease.admission);
        if self.lease.closed.load(Ordering::Acquire) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                OP,
            ));
        }
        // The public mobile contract admits up to 4 MiB per bounded recovery
        // read.  Keep this aligned with the RN bridge; callers paginate larger
        // retained evidence through the opaque handle.
        if maximum_bytes == 0 || maximum_bytes > 4 * 1024 * 1024 {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                OP,
            ));
        }
        let value = lock(&self.lease.selected)
            .get(&entry.reservation_id)
            .cloned()
            .filter(|value| value.scope == entry.scope && value.scope.producer == entry.producer)
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Stream,
                    OP,
                )
            })?;
        let start = usize::try_from(offset).map_err(|_| {
            DesktopError::new(BleErrorCode::ArgumentInvalid, BleErrorDomain::Stream, OP)
        })?;
        if start > value.canonical_bytes.len() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                OP,
            ));
        }
        let end = start
            .saturating_add(maximum_bytes as usize)
            .min(value.canonical_bytes.len());
        Ok(TerminalEvidenceRecoveryPage {
            offset,
            next_offset: u64::try_from(end).map_err(|_| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Stream,
                    OP,
                )
            })?,
            bytes: value.canonical_bytes[start..end].to_vec(),
            done: end == value.canonical_bytes.len(),
        })
    }

    /// Acknowledge only evidence selected through this trusted recovery lease.
    /// A staged row is first committed from the exact bytes already retained
    /// for this recipient, then retired by the same explicit acknowledgement.
    /// A runtime lease cannot select or retire either kind of row.
    pub fn acknowledge(&self, entry: &TerminalEvidenceRecoveryEntry) -> Result<(), DesktopError> {
        const OP: &str = "terminal-evidence-recovery.acknowledge";
        let _admission = lock(&self.lease.admission);
        if lock(&self.lease.acknowledged)
            .1
            .contains(&entry.reservation_id)
        {
            return Ok(());
        }
        let ledger = self.ledger(OP)?;
        // The selection is private to this recovery lease. Foreign bindings
        // keep opaque handles only; another lease cannot populate this map or
        // obtain the matching ledger authority.
        let retained = lock(&self.lease.selected)
            .get(&entry.reservation_id)
            .cloned()
            .filter(|candidate| {
                candidate.scope == entry.scope && candidate.scope.producer == entry.producer
            })
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Stream,
                    OP,
                )
            })?;
        let reference = match retained.state {
            ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Committed => {
                retained.reference.ok_or_else(|| {
                    DesktopError::new(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Stream,
                        OP,
                    )
                })?
            }
            ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Staged => {
                ledger
                    .commit_recovered(
                        &self.lease.id,
                        &retained.reservation_id,
                        &retained.canonical_bytes,
                        Sha256::digest(&retained.canonical_bytes).into(),
                    )
                    .map_err(terminal_recovery_error)?
                    .reference
            }
        };
        ledger
            .acknowledge_recovered(
                &self.lease.id,
                &retained.reservation_id,
                &reference,
                &entry.scope.producer,
                &self.host.terminal_evidence_recipient,
            )
            .map_err(terminal_recovery_error)?;
        lock(&self.lease.selected).remove(&entry.reservation_id);
        let mut acknowledged = lock(&self.lease.acknowledged);
        if acknowledged.1.insert(entry.reservation_id.clone()) {
            acknowledged.0.push_back(entry.reservation_id.clone());
            if acknowledged.0.len() > 256 {
                if let Some(expired) = acknowledged.0.pop_front() {
                    acknowledged.1.remove(&expired);
                }
            }
        }
        Ok(())
    }
}

/// Mobile capability truth is reported by the platform through the
/// provider; the core registers no desktop rows for a mobile radio.
fn register_mobile_capabilities(_core: &mut Central) -> Result<(), CoreError> {
    Ok(())
}

fn canonical_advertisement(advertisement: Advertisement) -> Option<PeerSnapshot> {
    let in_i8 = |value: Option<i16>| value.is_none_or(|v| (-128..=127).contains(&v));
    if advertisement.peer_id.is_empty()
        || !in_i8(advertisement.rssi)
        || !in_i8(advertisement.tx_power_level)
    {
        return None;
    }
    let service_uuids = advertisement
        .service_uuids
        .iter()
        .map(|uuid| canonical_uuid(uuid).ok())
        .collect::<Option<Vec<_>>>()?;
    let mut service_data = advertisement.service_data;
    for entry in &mut service_data {
        entry.uuid = canonical_uuid(&entry.uuid).ok()?;
    }
    let canonical_list = |list: Option<Vec<String>>| -> Option<Option<Vec<String>>> {
        match list {
            None => Some(None),
            Some(uuids) => uuids
                .iter()
                .map(|uuid| canonical_uuid(uuid).ok())
                .collect::<Option<Vec<_>>>()
                .map(Some),
        }
    };
    let extras = ubm_desktop::AdvertisementExtras {
        capture_timestamp_ms: advertisement.capture_timestamp_ms,
        cached_name: advertisement.cached_name,
        address_type: None,
        solicited_service_uuids: canonical_list(advertisement.solicited_service_uuids)?,
        overflow_service_uuids: canonical_list(advertisement.overflow_service_uuids)?,
        connectable: advertisement.connectable,
        appearance: advertisement.appearance,
        raw_record: advertisement.raw_record,
        // Android `ScanResult` and CoreBluetooth discovery callbacks carry
        // this one advertisement's data (finding 122).
        source: ubm_desktop::ObservationSource::Advertisement,
    };
    Some(PeerSnapshot {
        id: advertisement.peer_id,
        address: advertisement.address,
        service_uuids,
        rssi: advertisement.rssi,
        local_name: advertisement.local_name,
        manufacturer_data: advertisement.manufacturer_data,
        service_data,
        tx_power_level: advertisement.tx_power_level,
        extras,
    })
}

fn host_reset_error() -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
        "gatt.notification.source",
    )
    .with_platform(
        ubm_desktop::PlatformDetail::new("ubm", "source-observation-overflow")
            .with_message("Host observation capacity exceeded"),
    )
}

fn non_empty_or_null(items: Vec<Value>) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        Value::Array(items)
    }
}

fn uuid_list(uuids: Option<&[String]>) -> Value {
    non_empty_or_null(
        uuids
            .unwrap_or_default()
            .iter()
            .map(|uuid| Value::from(uuid.as_str()))
            .collect(),
    )
}

pub(crate) fn advertisement_record(
    snapshot: &PeerSnapshot,
    observed_at_ms: u64,
    membership: &str,
    start_operation_id: &str,
) -> Value {
    object(vec![
        ("t", Value::from("adv")),
        ("operationId", Value::from(membership)),
        ("startOperationId", Value::from(start_operation_id)),
        ("peerId", Value::from(snapshot.id.as_str())),
        ("localName", opt_text(snapshot.local_name.as_deref())),
        ("rssi", snapshot.rssi.map_or(Value::Null, Value::from)),
        (
            "txPower",
            snapshot.tx_power_level.map_or(Value::Null, Value::from),
        ),
        (
            "serviceUuids",
            non_empty_or_null(
                snapshot
                    .service_uuids
                    .iter()
                    .map(|uuid| Value::from(uuid.as_str()))
                    .collect(),
            ),
        ),
        (
            "manufacturerData",
            non_empty_or_null(
                snapshot
                    .manufacturer_data
                    .iter()
                    .map(|entry| {
                        object(vec![
                            ("companyId", Value::from(entry.company_id)),
                            (
                                "payloadB64",
                                Value::from(wire::encode_base64(&entry.payload)),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "serviceData",
            non_empty_or_null(
                snapshot
                    .service_data
                    .iter()
                    .map(|entry| {
                        object(vec![
                            ("uuid", Value::from(entry.uuid.as_str())),
                            (
                                "payloadB64",
                                Value::from(wire::encode_base64(&entry.payload)),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "connectable",
            snapshot.extras.connectable.map_or(Value::Null, Value::from),
        ),
        (
            "solicitedServiceUuids",
            uuid_list(snapshot.extras.solicited_service_uuids.as_deref()),
        ),
        (
            "overflowServiceUuids",
            uuid_list(snapshot.extras.overflow_service_uuids.as_deref()),
        ),
        (
            "appearance",
            snapshot.extras.appearance.map_or(Value::Null, Value::from),
        ),
        (
            "rawRecordB64",
            snapshot
                .extras
                .raw_record
                .as_deref()
                .map_or(Value::Null, |bytes| Value::from(wire::encode_base64(bytes))),
        ),
        ("observedAtMs", Value::from(observed_at_ms)),
        (
            "sourceTimestampMs",
            snapshot
                .extras
                .capture_timestamp_ms
                .map_or(Value::Null, Value::from),
        ),
    ])
}

pub(crate) fn adapter_value(
    snapshot: &AdapterSnapshot,
    updated_at: u64,
    attachment: &AttachmentTuple,
) -> Value {
    object(vec![
        ("availability", Value::from(snapshot.availability.as_str())),
        (
            "authorization",
            Value::from(snapshot.authorization.as_str()),
        ),
        ("power", Value::from(snapshot.power.as_str())),
        ("safeReason", opt_text(snapshot.safe_reason.as_deref())),
        ("updatedAt", Value::from(updated_at)),
        (
            "backendGeneration",
            Value::from(attachment.backend_generation().as_str()),
        ),
        (
            "adapterGeneration",
            Value::from(attachment.adapter_generation().as_str()),
        ),
    ])
}

pub(crate) fn security_value(state: &SecurityState) -> Value {
    object(vec![
        ("bond", Value::from(state.bond.as_str())),
        ("encryption", Value::from(state.encryption.as_str())),
        ("authentication", Value::from(state.authentication.as_str())),
        (
            "secureConnections",
            Value::from(state.secure_connections.as_str()),
        ),
        (
            "pairingPossible",
            state.pairing_possible.map_or(Value::Null, Value::from),
        ),
    ])
}

fn peer_connection(record: Option<&PeerRecord>) -> &'static str {
    match record.and_then(|record| record.connection_state) {
        Some(ConnectionState::Connected) => "connected",
        Some(ConnectionState::Disconnected | ConnectionState::Lost | ConnectionState::Invalid) => {
            "disconnected"
        }
        Some(ConnectionState::Connecting | ConnectionState::Disconnecting) | None => "unknown",
    }
}

fn matches_member(member: &ScanMember, snapshot: &PeerSnapshot) -> bool {
    if member
        .deadline
        .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    {
        return false;
    }
    // A platform callback may be a partial update. In particular, an
    // absent service UUID does not negate evidence from an earlier callback.
    // Deliver it to the session's evidence matcher, which owns that history.
    let address_match = member.device_addresses.is_empty()
        || member.device_addresses.iter().any(|address| {
            address.eq_ignore_ascii_case(&snapshot.id)
                || snapshot
                    .address
                    .as_deref()
                    .is_some_and(|own| own.eq_ignore_ascii_case(address))
        });
    address_match
}

impl HostInner {
    fn capture_advertisement(
        &self,
        advertisement: Advertisement,
        expected: Option<RequestId>,
    ) -> Result<(), IngressClass> {
        let mut ingress = lock(&self.scan_ingress);
        let Some(generation) = ingress.generation else {
            self.radio.note_drop(IngressClass::Advertisement);
            return Err(IngressClass::Advertisement);
        };
        if expected.is_some_and(|expected| expected != generation)
            || ingress.failed_generation == Some(generation)
            || ingress.queue.len() >= crate::foreign::ADVERTISEMENT_INGRESS_CAP
        {
            self.radio.note_drop(IngressClass::Advertisement);
            return Err(IngressClass::Advertisement);
        }
        let mut members: Vec<_> = lock(&self.scan_members)
            .iter()
            .map(|(id, member)| (*id, member.clone()))
            .collect();
        if let Some((id, member)) = &ingress.pending {
            members.retain(|(existing, _)| existing != id);
            members.push((*id, member.clone()));
        }
        if members.is_empty() {
            self.radio.note_drop(IngressClass::Advertisement);
            return Err(IngressClass::Advertisement);
        }
        let Some(snapshot) = canonical_advertisement(advertisement) else {
            self.radio.note_drop(IngressClass::Advertisement);
            return Err(IngressClass::Advertisement);
        };
        members.retain(|(_, member)| matches_member(member, &snapshot));
        self.radio
            .push_event(RadioEvent::Advertisement(snapshot.clone()))?;
        for (id, _) in &members {
            *ingress.pending_by_session.entry(*id).or_default() += 1;
        }
        ingress
            .queue
            .push_back(ScanIngressItem::Advertisement(CapturedAdvertisement {
                generation,
                snapshot,
                members,
                observed_at: self.now_ms(),
            }));
        drop(ingress);
        self.signals.push(HostSignal::Advertisements);
        Ok(())
    }
    /// Reserve the reset terminal at the synchronous central observation, before
    /// stop, disposal or sealing can retire its original recipients.
    fn capture_reset_scan(
        &self,
        previous: &AttachmentTuple,
        ended: Option<&OperationId>,
        cause: ubm_desktop::AdapterLossCause,
    ) -> bool {
        let Some(ended) = ended else {
            return false;
        };
        let mut ingress = lock(&self.scan_ingress);
        if ingress.attachment.as_ref() != Some(previous)
            || ingress
                .operation
                .as_ref()
                .is_some_and(|operation| operation != ended)
            || (ingress.operation.is_none() && ingress.pending.is_none())
        {
            return false;
        }
        let Some(generation) = ingress.generation else {
            return false;
        };
        if ingress.failed_generation == Some(generation) {
            return true;
        }
        let code = match cause {
            ubm_desktop::AdapterLossCause::PoweredOff => BleErrorCode::AdapterPoweredOff,
            ubm_desktop::AdapterLossCause::Resetting => BleErrorCode::AdapterResetting,
            _ => BleErrorCode::AdapterUnavailable,
        };
        let error = DesktopError::new(code, BleErrorDomain::Adapter, "scan.start")
            .with_detail(cause.as_str());
        self.capture_scan_failure_locked(&mut ingress, error, Some(generation), "adapter")
            .is_ok()
    }

    fn capture_scan_failure(
        &self,
        error: DesktopError,
        expected: Option<RequestId>,
    ) -> Result<(), (IngressClass, DesktopError)> {
        let mut ingress = lock(&self.scan_ingress);
        self.capture_scan_failure_locked(&mut ingress, error, expected, "radio")
    }

    fn capture_scan_failure_locked(
        &self,
        ingress: &mut ScanIngress,
        error: DesktopError,
        expected: Option<RequestId>,
        source: &'static str,
    ) -> Result<(), (IngressClass, DesktopError)> {
        if self.shut_down.load(Ordering::SeqCst) {
            return Err((IngressClass::Control, error));
        }
        let Some(generation) = ingress.generation else {
            self.radio.note_drop(IngressClass::Control);
            return Err((IngressClass::Control, error));
        };
        if expected.is_some_and(|id| id != generation)
            || ingress.failed_generation == Some(generation)
        {
            self.radio.note_drop(IngressClass::Control);
            return Err((IngressClass::Control, error));
        }
        let mut members: Vec<_> = lock(&self.scan_members)
            .iter()
            .map(|(id, member)| (*id, member.clone()))
            .collect();
        if let Some((id, member)) = &ingress.pending {
            members.retain(|(existing, _)| existing != id);
            members.push((*id, member.clone()));
        }
        // Reserve every v3 destination before accepting the callback. A
        // refusal drops only uncommitted reservations and returns the original
        // error; no recipient has acquired a partial failure cut.
        let mut transfers = Vec::new();
        let mut shared = None;
        for (id, member) in &members {
            let Some(session) = self
                .session(*id)
                .or_else(|| ingress.retired_sessions.get(id).cloned())
            else {
                return Err((IngressClass::Control, error));
            };
            let version = *lock(&session.scan_end_version);
            if version == Some(3) {
                let identity = if let Some(identity) = shared {
                    identity
                } else {
                    let host = match ingress.failure_host_identity {
                        Some(host) => host,
                        None => {
                            let mut host = [0; 16];
                            if getrandom::fill(&mut host).is_err() {
                                return Err((IngressClass::Control, error));
                            }
                            ingress.failure_host_identity = Some(host);
                            host
                        }
                    };
                    let mut random = [0; 48];
                    if getrandom::fill(&mut random).is_err() {
                        return Err((IngressClass::Control, error));
                    }
                    let id = |offset: usize| -> [u8; 16] {
                        random[offset..offset + 16]
                            .try_into()
                            .expect("fixed identity length")
                    };
                    let identity = ScanFailureSharedIdentity {
                        host,
                        scan: id(0),
                        source_generation: id(16),
                        occurrence: id(32),
                    };
                    shared = Some(identity);
                    identity
                };
                let Some(transfer) = ScanFailureTransfer::reserve(
                    session, member, generation, source, &error, identity,
                ) else {
                    return Err((IngressClass::Control, error));
                };
                transfers.push(transfer);
            }
        }
        let cut = Arc::new(());
        let diagnostic = error.detail().map(str::to_owned);
        let admission = match self.signals.try_push_scan_failure(
            error,
            generation,
            members.clone(),
            Arc::clone(&cut),
            source,
        ) {
            Ok(admission) => admission,
            Err(error) => {
                // The terminal could not enter even the reserved slot. Do not
                // continue admitting members to a source known to have failed.
                ingress.failed_generation = Some(generation);
                self.radio.note_drop(IngressClass::Control);
                self.shut_down.store(true, Ordering::SeqCst);
                self.ordered_wakes.close();
                self.signals
                    .fail_source_ingress("scan", "scan.source-ingress", diagnostic);
                return Err((IngressClass::Control, error));
            }
        };
        for (id, _) in &members {
            *ingress.pending_by_session.entry(*id).or_default() += 1;
        }
        ingress
            .failure_transfers
            .insert(Arc::as_ptr(&cut) as usize, transfers);
        ingress.queue.push_back(ScanIngressItem::FailureCut(cut));
        ingress.failed_generation = Some(generation);
        if admission == ScanFailureAdmission::Emergency {
            // Keep the exact terminal and its fence in one final bounded
            // slot, then retire this saturated owner after it is reported.
            self.shut_down.store(true, Ordering::SeqCst);
            self.ordered_wakes.close();
            self.signals
                .fail_source_ingress("scan", "scan.source-ingress", diagnostic);
        }
        Ok(())
    }

    fn finish_scan_ingress(&self, session_id: u64) {
        let mut ingress = lock(&self.scan_ingress);
        let count = ingress
            .pending_by_session
            .get_mut(&session_id)
            .expect("captured scan ingress has an owner");
        *count -= 1;
        if *count == 0 {
            ingress.pending_by_session.remove(&session_id);
            if ingress
                .retired_sessions
                .get(&session_id)
                .is_some_and(|session| session.outbox.pending_scan_failure_owners() == 0)
                && !self.has_scan_terminal_evidence_debt(session_id)
            {
                ingress.retired_sessions.remove(&session_id);
            }
        }
    }

    /// Serialize the cutoff with callback-time recipient capture. An in-flight
    /// producer keeps its recipient pending until it has completed delivery.
    pub(crate) fn seal_scan_ingress(
        &self,
        session: &SessionState,
    ) -> Option<ubm_desktop::continuation_outbox::AfterCutoffLoss> {
        let ingress = match self.scan_ingress.try_lock() {
            Ok(ingress) => ingress,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if ingress.pending_by_session.contains_key(&session.id) {
            return None;
        }
        session.outbox.try_seal()
    }

    /// Reserve an owner-wide group for v2 dispatch. The legacy open path must
    /// not call this: it cannot prove when a mapping is safe to forget.
    pub(crate) fn reserve_ordered_group(
        &self,
        collection_id: u64,
    ) -> Result<Option<crate::ordered_open_slots::GroupMapping>, crate::ordered_open_slots::Error>
    {
        let _admission = lock(&self.sessions);
        if self.shut_down.load(Ordering::SeqCst)
            || self.session_admission_closed.load(Ordering::SeqCst)
        {
            return Err(crate::ordered_open_slots::Error::OwnerRetired);
        }
        self.ordered_group_owner
            .as_ref()
            .map(|(owner, host)| owner.allocate_group(*host, collection_id))
            .transpose()
    }

    pub(crate) fn ordered_group(
        &self,
        collection_id: u64,
    ) -> Option<crate::ordered_open_slots::GroupMapping> {
        self.ordered_group_owner
            .as_ref()
            .and_then(|(owner, host)| owner.group(*host, collection_id))
    }

    /// Only a reservation whose senders have all been fenced may use this.
    pub(crate) fn forget_unsubmitted_ordered_group(
        &self,
        mapping: crate::ordered_open_slots::GroupMapping,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        let (owner, host) = self
            .ordered_group_owner
            .as_ref()
            .ok_or(crate::ordered_open_slots::Error::IdentityMismatch)?;
        if mapping.host != *host {
            return Err(crate::ordered_open_slots::Error::IdentityMismatch);
        }
        owner.forget_unsubmitted_group(mapping, mapping.native_group_id)
    }

    async fn fail_pump(self: &Arc<Self>, error: Value) {
        // Close admission before waiting for disk or native cleanup. A dead
        // pump is not a usable owner, even when disposal must be retried.
        {
            let _sessions = lock(&self.sessions);
            self.shut_down.store(true, Ordering::SeqCst);
        }
        self.continuation_closed.store(true, Ordering::SeqCst);
        if let Some(executor) = self.continuation.get() {
            executor.stop_recovery();
        }
        *lock(&self.ingress_failure) = Some(serde_json::json!({"error":error}));
        let owned = Arc::clone(self);
        let result = ubm_desktop::continuation_journal::run_blocking_result(move || {
            for session in owned.session_list() {
                session.outbox.fail_collection_worker();
                session.outbox.push_ingress_drop(IngressClass::Control);
            }
            let routes = lock(&owned.routes).clone();
            for (scope, entries) in routes {
                for route in entries {
                    if route.terminal.is_none() {
                        owned.mark_ended(&scope, &route, ("source-failed", 0, 0, None));
                        owned.end_route(&route, ("source-failed", 0, 0, None), false);
                    }
                }
            }
            // Keep membership ownership until authoritative native cleanup.
            let scans = lock(&owned.scan_members).clone();
            for (id, member) in scans {
                if let Some(session) = owned.session(id) {
                    if *lock(&session.scan_end_version) == Some(2) {
                        // An ingress worker failure is not an adapter or radio
                        // observation and cannot be represented as v2 without
                        // inventing its source. Report the lost control.
                        session.outbox.push_ingress_drop(IngressClass::Control);
                    } else {
                        session.outbox.push_control(serde_json::json!({"t":"scan-end","operationId":member.membership,"reason":"source-failed"}));
                    }
                }
            }
            Ok(())
        }).await;
        if let Err(error) = result {
            if let Some(record) = lock(&self.ingress_failure).as_mut() {
                record["cleanupFailure"] = error;
            }
        }
        // The pump must return before shutdown can join it. A separate task
        // owns the retry and records its result, even if another caller is
        // already holding the shutdown lock.
        let owned = Arc::clone(self);
        self.runtime.spawn(async move {
            let receipt = MobileHost { inner: Arc::clone(&owned) }.shutdown().await;
            if let Some(record) = lock(&owned.ingress_failure).as_mut() {
                match serde_json::from_str::<Value>(&receipt) {
                    Ok(cleanup) => record["cleanup"] = cleanup,
                    Err(error) => record["cleanupFailure"] = serde_json::json!({"code":"protocol.malformed","domain":"core","operation":"ubm-mobile.ingress.cleanup","detail":error.to_string()}),
                }
            }
        });
    }
    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::SeqCst)
    }

    pub(crate) fn session_list(&self) -> Vec<Arc<SessionState>> {
        lock(&self.sessions).values().cloned().collect()
    }

    fn session(&self, id: u64) -> Option<Arc<SessionState>> {
        lock(&self.sessions).get(&id).cloned()
    }

    fn scan_session(&self, id: u64) -> Option<Arc<SessionState>> {
        // Synchronize lookup with removal so ownership cannot disappear
        // between the active and retired registries.
        let ingress = lock(&self.scan_ingress);
        self.session(id)
            .or_else(|| ingress.retired_sessions.get(&id).cloned())
    }

    pub(crate) fn broadcast(&self, record: &Value) {
        for session in self.session_list() {
            session.outbox.push_control(record.clone());
        }
    }

    /// One peer record for the wire (`peers.*`).
    pub(crate) fn peer_value(
        &self,
        peer_id: &str,
        core: Option<&PeerRecord>,
        source_override: Option<&'static str>,
    ) -> Value {
        let info = lock(&self.directory).get(peer_id).cloned();
        let restored = lock(&self.restored).get(peer_id).cloned();
        let bond = lock(&self.security)
            .get(peer_id)
            .map(|entry| entry.state.bond)
            .map_or("unknown", |bond| match bond {
                BondState::Bonded => "bonded",
                BondState::NotBonded => "not-bonded",
                BondState::Unsupported => "unsupported",
                BondState::Bonding | BondState::Unknown => "unknown",
            });
        let connection = peer_connection(core);
        let source = source_override
            .or(info.as_ref().map(|info| info.source))
            .or(restored.as_ref().map(|_| "restored"))
            .unwrap_or("app-reference");
        let name = info
            .as_ref()
            .and_then(|info| info.name.clone())
            .or(restored.and_then(|peer| peer.name));
        object(vec![
            ("peerId", Value::from(peer_id)),
            ("name", opt_text(name.as_deref())),
            (
                "rssi",
                info.as_ref()
                    .and_then(|info| info.rssi)
                    .map_or(Value::Null, Value::from),
            ),
            ("source", Value::from(source)),
            (
                "reachability",
                Value::from(if connection == "connected" {
                    "reachable"
                } else {
                    "unknown"
                }),
            ),
            ("connection", Value::from(connection)),
            ("bond", Value::from(bond)),
            (
                "lastSeenAtMonotonicMs",
                info.and_then(|info| info.last_seen_ms)
                    .map_or(Value::Null, Value::from),
            ),
        ])
    }

    /// Record a peer the host learned about outside a scan.
    pub(crate) fn note_peer(&self, peer_id: &str, name: Option<String>, source: &'static str) {
        let mut directory = lock(&self.directory);
        let entry = directory.entry(peer_id.to_owned()).or_insert(PeerInfo {
            name: None,
            rssi: None,
            last_seen_ms: None,
            source,
        });
        if entry.name.is_none() {
            entry.name = name;
        }
    }

    async fn handle(self: &Arc<Self>, signal: HostSignal) {
        match signal {
            HostSignal::ScanDeadlines => self.expire_scan_members(),
            HostSignal::Advertisements => {
                // The central still owns peer resolution and its bounded
                // queue; mobile fanout uses the callback-time capture instead.
                let mut drained = 0;
                for _ in 0..ADVERTISEMENT_BATCH {
                    if self.central.take_advertisement().await.is_none() {
                        break;
                    }
                    drained += 1;
                }
                let mut delivered = 0;
                while delivered < ADVERTISEMENT_BATCH {
                    let record = {
                        let mut ingress = lock(&self.scan_ingress);
                        if ingress.pending.is_some()
                            && ingress.queue.front().is_some_and(|entry| {
                                matches!(entry, ScanIngressItem::Advertisement(entry) if Some(entry.generation) == ingress.generation)
                            })
                        {
                            None
                        } else if matches!(ingress.queue.front(), Some(ScanIngressItem::Advertisement(_))) {
                            match ingress.queue.pop_front() {
                                Some(ScanIngressItem::Advertisement(record)) => Some(record),
                                _ => unreachable!("the front was an advertisement"),
                            }
                        } else {
                            None
                        }
                    };
                    let Some(record) = record else {
                        break;
                    };
                    self.route_advertisement(record);
                    delivered += 1;
                }
                if delivered == ADVERTISEMENT_BATCH || drained == ADVERTISEMENT_BATCH {
                    self.signals.push(HostSignal::Advertisements);
                }
            }
            HostSignal::Values => {
                // One bounded batch per marker turn, so lifecycle and
                // current-state signals waiting behind it are not starved;
                // leftovers requeue the marker for another turn.
                let batch = self.signals.take_value_batch(VALUE_SCOPE_BATCH);
                for scope in &batch {
                    if self.flush_scope(scope).await {
                        self.signals.push_value(scope.clone());
                    }
                }
                self.signals.requeue_values_if_dirty();
            }
            HostSignal::Lifecycle(event) => self.route_lifecycle(event).await,
            HostSignal::Adapter(snapshot, updated_at, attachment) => {
                let record = object(vec![
                    ("t", Value::from("adapter")),
                    ("state", adapter_value(&snapshot, updated_at, &attachment)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::AdapterReset(attachment, ended_scan, cause, mut resources) => {
                {
                    let mut ingress = lock(&self.scan_ingress);
                    if ingress.pending.is_some() || !ingress.deferred_failures.is_empty() {
                        if let Some(HostSignal::AdapterReset(
                            current,
                            ended,
                            current_cause,
                            captured,
                        )) = &mut ingress.deferred_reset
                        {
                            captured.append(&mut resources);
                            *current = attachment;
                            if ended_scan.is_some() {
                                *current_cause = cause;
                                *ended = ended_scan.clone();
                            }
                        } else {
                            ingress.deferred_reset = Some(HostSignal::AdapterReset(
                                attachment, ended_scan, cause, resources,
                            ));
                        }
                        return;
                    }
                }
                self.adapter_reset(&attachment, ended_scan, cause, resources)
                    .await;
            }
            #[cfg(test)]
            HostSignal::ScanFailed(detail) => {
                self.scan_failed(
                    DesktopError::new(
                        BleErrorCode::ScanStartFailed,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )
                    .with_detail(detail),
                    None,
                    Vec::new(),
                    "radio",
                    &HashSet::new(),
                )
                .await
            }
            HostSignal::ScanFailedScoped(error, generation, members, cut, source) => {
                // A pending start owns its captured observations until its
                // radio admission settles; never route through that boundary.
                {
                    let mut ingress = lock(&self.scan_ingress);
                    if ingress.pending.is_some() || !ingress.deferred_failures.is_empty() {
                        ingress
                            .deferred_failures
                            .push_back(HostSignal::ScanFailedScoped(
                                error, generation, members, cut, source,
                            ));
                        return;
                    }
                    let first_cut = ingress.queue.iter().find_map(|item| match item {
                        ScanIngressItem::FailureCut(found) => Some(found),
                        ScanIngressItem::Advertisement(_) => None,
                    });
                    if first_cut.is_some_and(|first| !Arc::ptr_eq(first, &cut)) {
                        // A previously deferred failure resumed after this
                        // signal was popped. Its cut still owns the prefix.
                        self.signals
                            .requeue_scan_failure(HostSignal::ScanFailedScoped(
                                error, generation, members, cut, source,
                            ));
                        return;
                    }
                }
                loop {
                    let item = lock(&self.scan_ingress).queue.pop_front();
                    match item {
                        Some(ScanIngressItem::Advertisement(record)) => {
                            self.route_advertisement(record)
                        }
                        Some(ScanIngressItem::FailureCut(found)) if Arc::ptr_eq(&found, &cut) => {
                            break;
                        }
                        // Earlier failure cuts cannot be crossed: signals are handled FIFO.
                        _ => panic!("scan failure cut missing or out of order"),
                    }
                }
                let key = Arc::as_ptr(&cut) as usize;
                let mut transfers = {
                    let mut ingress = lock(&self.scan_ingress);
                    let transfers = ingress
                        .failure_transfers
                        .remove(&key)
                        .expect("admitted failure owns its reservations");
                    ingress.failure_transfers_in_flight += 1;
                    transfers
                };
                let mut refused = false;
                for transfer in &mut transfers {
                    if transfer.captured {
                        continue;
                    }
                    let result = transfer
                        .session
                        .outbox
                        .commit_scan_failure(&mut transfer.reservation, &mut transfer.occurrence);
                    transfer.captured = transfer.occurrence.is_none();
                    if let Err(problem) = result {
                        eprintln!(
                            "ubm-mobile: scan failure outbox transfer needs recovery: {problem:?}"
                        );
                        if !transfer.captured {
                            refused = true;
                            break;
                        }
                    }
                }
                if refused {
                    // Keep the cut, the exact error and every remaining source
                    // owned. A bounded delayed wake retries without depending
                    // on another platform callback or spinning the pump.
                    let mut ingress = lock(&self.scan_ingress);
                    ingress.failure_transfers_in_flight -= 1;
                    ingress
                        .queue
                        .push_front(ScanIngressItem::FailureCut(Arc::clone(&cut)));
                    ingress.failure_transfers.insert(key, transfers);
                    ingress
                        .deferred_failures
                        .push_back(HostSignal::ScanFailedScoped(
                            error, generation, members, cut, source,
                        ));
                    if !ingress.transfer_retry_scheduled {
                        ingress.transfer_retry_scheduled = true;
                        let owner = Arc::downgrade(self);
                        self.runtime.spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let Some(owner) = owner.upgrade() else {
                                return;
                            };
                            let mut ingress = lock(&owner.scan_ingress);
                            ingress.transfer_retry_scheduled = false;
                            if ingress.pending.is_none() {
                                let failures = std::mem::take(&mut ingress.deferred_failures);
                                let reset = ingress.deferred_reset.take();
                                let deadlines = std::mem::take(&mut ingress.deferred_deadlines);
                                owner.signals.resume_scan_signals(failures, reset);
                                if deadlines {
                                    owner.signals.push(HostSignal::ScanDeadlines);
                                }
                            }
                        });
                    }
                    return;
                }
                let recipients = transfers
                    .iter()
                    .map(|transfer| transfer.session.id)
                    .collect();
                for mut transfer in transfers {
                    if let Err(problem) = transfer
                        .session
                        .outbox
                        .finish_scan_failure_producer(&mut transfer.reservation)
                    {
                        eprintln!(
                            "ubm-mobile: scan failure producer release needs recovery: {problem:?}"
                        );
                    }
                    // A failed preparation remains discoverable as raw
                    // evidence with its fence and exact source intact.
                }
                lock(&self.scan_ingress).failure_transfers_in_flight -= 1;
                self.scan_failed(error, Some(generation), members, source, &recipients)
                    .await;
                self.signals.finish_scan_failure();
                // A marker consumed before this cut may have represented
                // observations captured for a later physical generation.
                if !lock(&self.scan_ingress).queue.is_empty() {
                    self.signals.push(HostSignal::Advertisements);
                }
            }
            HostSignal::Security(peer_id, state) => {
                let record = object(vec![
                    ("t", Value::from("security")),
                    ("peerId", Value::from(peer_id.as_str())),
                    ("state", security_value(&state)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::SecurityFailed(peer_id, error) => {
                self.broadcast(&object(vec![
                    ("t", Value::from("security-failed")),
                    ("peerId", opt_text(peer_id.as_deref())),
                    ("error", Value::Object(wire::error_object(&error))),
                ]));
            }
            HostSignal::WriteReadiness(peer_id, generation, ready) => {
                let record = object(vec![
                    ("t", Value::from("readiness")),
                    ("peerId", Value::from(peer_id.as_str())),
                    ("connectionGeneration", opt_text(generation.as_deref())),
                    ("ready", Value::from(ready)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::Restored(peers) => {
                let record = object(vec![
                    ("t", Value::from("restored")),
                    (
                        "peers",
                        Value::Array(
                            peers
                                .iter()
                                .map(|peer| {
                                    object(vec![
                                        ("peerId", Value::from(peer.peer_id.as_str())),
                                        ("name", opt_text(peer.name.as_deref())),
                                        ("connected", Value::Bool(peer.connected)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ]);
                self.broadcast(&record);
            }
            HostSignal::IngressDrop(class, count) => {
                self.record_ingress_drop(class, count).await;
                for session in self.session_list() {
                    session.outbox.push_ingress_drop_count(class, count);
                }
            }
        }
    }

    async fn record_ingress_drop(&self, class: IngressClass, count: u64) {
        if class != IngressClass::Notification || count == 0 {
            return;
        }
        let routes: Vec<(u64, String, RecordedRoute)> = {
            // Hold both locks while snapshotting so promotion cannot create a
            // gap between the pre-enable and active loss destinations.
            let active = lock(&self.routes);
            let pending = lock(&self.pending_recordings);
            active
                .values()
                .flatten()
                .filter_map(|route| {
                    route
                        .recorded
                        .clone()
                        .map(|recorded| (route.session_id, route.consumer.clone(), recorded))
                })
                .chain(pending.iter().map(|((session, consumer), recorded)| {
                    (*session, consumer.clone(), recorded.clone())
                }))
                .collect()
        };
        let mut seen = HashSet::new();
        for (session_id, consumer, recorded) in routes {
            let identity = Arc::as_ptr(&recorded.journal) as usize;
            if !seen.insert(identity) {
                self.notify_recorded_change(session_id, &consumer, &recorded);
                continue;
            }
            let _order = recorded.order.lock().await;
            let metadata =
                serde_json::json!({"session":recorded.metadata["session"],"consumer":null});
            let loss = serde_json::json!({"t":"ingress-drop","class":"notification","count":count});
            let impact = ubm_desktop::continuation_journal::RecordedLoss {
                items: count,
                bytes: None,
            };
            let _ = recorded.journal.append_recorded_batch(
                &[(&metadata, &loss)],
                &[impact],
                Default::default(),
            );
            self.notify_recorded_change(session_id, &consumer, &recorded);
        }
        self.recorded_drops_settled
            .fetch_add(count, Ordering::SeqCst);
        self.recorded_drops_notify.notify_waiters();
    }

    /// Before retiring a failed pending subscription, wait for every global
    /// notification-drop signal already counted at that cutoff to reach its
    /// registered journal destination. No peer attribution is inferred.
    pub(crate) async fn settle_recorded_ingress_drops(&self) -> Result<(), DesktopError> {
        let target = self.radio.counters().notification_drops;
        let settled = async {
            loop {
                let notified = self.recorded_drops_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.recorded_drops_settled.load(Ordering::SeqCst) >= target {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(ubm_desktop::LIVENESS_OP, settled)
            .await
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Platform,
                    "gatt.subscribe-recorded.ingress-loss-settlement",
                )
            })
    }

    fn route_advertisement(&self, record: CapturedAdvertisement) {
        let CapturedAdvertisement {
            snapshot,
            members,
            observed_at: now,
            ..
        } = record;
        {
            let mut directory = lock(&self.directory);
            let entry = directory.entry(snapshot.id.clone()).or_insert(PeerInfo {
                name: None,
                rssi: None,
                last_seen_ms: None,
                source: "scan-observed",
            });
            if snapshot.local_name.is_some() {
                entry.name.clone_from(&snapshot.local_name);
            } else if snapshot.extras.cached_name.is_some() {
                entry.name.clone_from(&snapshot.extras.cached_name);
            }
            entry.rssi = snapshot.rssi.or(entry.rssi);
            entry.last_seen_ms = Some(now);
        }
        for (session_id, member) in members {
            if let Some(session) = self.scan_session(session_id)
                && session
                    .outbox
                    .push_data(advertisement_record(
                        &snapshot,
                        now,
                        &member.membership,
                        &member.start_operation_id,
                    ))
                    .is_err()
            {
                session
                    .outbox
                    .push_ingress_drop(IngressClass::Advertisement);
            }
            self.finish_scan_ingress(session_id);
        }
    }

    fn end_route(
        &self,
        route: &Route,
        (reason, dropped_items, dropped_bytes, error): StreamEnd,
        loss_accounted: bool,
    ) {
        if let Some(recorded) = &route.recorded
            && !loss_accounted
        {
            let mut terminal = serde_json::json!({"t":"stream-end","consumer":route.consumer,
                "reason":reason,"droppedItems":dropped_items,"droppedBytes":dropped_bytes});
            if let Some(error) = &error {
                terminal["error"] = Value::Object(wire::error_object(error));
            }
            let impact = ubm_desktop::continuation_journal::RecordedLoss {
                items: dropped_items,
                bytes: Some(dropped_bytes),
            };
            let _ = recorded.journal.append_recorded_batch(
                &[(&recorded.metadata, &terminal)],
                &[impact],
                Default::default(),
            );
            self.notify_recorded_change(route.session_id, &route.consumer, recorded);
        }
        let mut record = object(vec![
            ("t", Value::from("stream-end")),
            ("consumer", Value::from(route.consumer.as_str())),
            ("reason", Value::from(reason)),
            ("droppedItems", Value::from(dropped_items)),
            ("droppedBytes", Value::from(dropped_bytes)),
        ]);
        if let Some(error) = &error {
            record["error"] = Value::Object(wire::error_object(error));
        }
        // A captured route can outlive its JS session during disposal. Its
        // terminal evidence still belongs to the native reservation and must
        // be staged for explicit recovery; publication itself only touches a
        // live session inside `publish_subscription_terminal_or_retain`.
        self.publish_subscription_terminal_or_retain(route.clone(), record);
    }

    /// Stage and commit the original terminal before publishing it to JS. A
    /// commit failure retains the exact already-staged bytes and route owner;
    /// reconcile retries it, so an async drain ACK can never outrun durable
    /// evidence.
    fn publish_subscription_terminal_or_retain(&self, route: Route, mut record: Value) {
        let Some(scope) = &route.terminal_evidence_scope else {
            if let Some(session) = self.session(route.session_id) {
                session.outbox.push_control(record);
            }
            return;
        };
        let result = (|| {
            let ledger = self
                .terminal_evidence_ledger()
                .ok_or(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
            let reservation = ledger.attach(&TerminalEvidenceScope {
                producer: "subscription".to_owned(),
                manager_id: scope.manager_id.clone(),
                operation_id: scope.operation_id.clone(),
                incarnation: scope.incarnation.clone(),
                scope_utf16le: scope.scope_utf16le.clone(),
            })?;
            let canonical = serde_json::to_vec(&record).map_err(|_| {
                ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Invalid(
                    "subscription terminal",
                )
            })?;
            ledger.stage(
                &reservation.reservation_id,
                &self.terminal_evidence_recipient,
                &canonical,
            )?;
            #[cfg(any(test, feature = "test-support"))]
            if self
                .subscription_terminal_evidence_commit_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Busy);
            }
            let committed = ledger.commit(
                &reservation.reservation_id,
                &canonical,
                Sha256::digest(&canonical).into(),
            )?;
            Ok::<_, ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError>((
                serde_json::json!({
                    "producer":"subscription", "kind":"committed", "ref":committed.reference,
                    "byteLength":committed.byte_length, "digest":committed.digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                    "formatVersion":committed.format_version, "durability":"committed",
                    "scope":terminal_evidence_scope_value(scope)
                }),
                ledger,
                reservation.reservation_id,
            ))
        })();
        match result {
            Ok((evidence, _ledger, reservation_id)) => {
                let publication = PublishedSubscriptionTerminalEvidence {
                    route: route.clone(),
                    record: record.clone(),
                    reservation_id: reservation_id.clone(),
                };
                lock(&self.subscription_terminal_evidence_publications)
                    .insert(reservation_id.clone(), publication);
                #[cfg(test)]
                if let Some(callback) = lock(&self.subscription_terminal_evidence_after_commit)
                    .as_ref()
                    .cloned()
                {
                    callback();
                }
                if let Some(session) = self.session(route.session_id) {
                    record
                        .as_object_mut()
                        .expect("terminal record object")
                        .insert("evidence".to_owned(), evidence);
                    session.outbox.push_control(record);
                }
                // Session teardown can race both the lookup and the outbox
                // push. It takes the publication record before abandoning it;
                // if it won that race there is nothing left for this producer
                // to hand off. Conversely, a removed session observed here
                // transfers the exact committed reservation to recovery.
                if self.session(route.session_id).is_none()
                    && let Some(publication) =
                        lock(&self.subscription_terminal_evidence_publications)
                            .remove(&reservation_id)
                {
                    self.abandon_subscription_terminal_evidence_or_retain(publication);
                }
            }
            Err(_) => lock(&self.subscription_terminal_evidence_debts)
                .push(SubscriptionTerminalEvidenceDebt { route, record }),
        }
    }

    fn abandon_subscription_terminal_evidence_or_retain(
        &self,
        publication: PublishedSubscriptionTerminalEvidence,
    ) {
        let handed_off = self
            .terminal_evidence_ledger()
            .map(|ledger| ledger.abandon(&publication.reservation_id))
            .is_some_and(|result| {
                result.is_ok()
                    || matches!(
                        result,
                        Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Missing)
                    )
            });
        if !handed_off {
            lock(&self.subscription_terminal_evidence_debts).push(
                SubscriptionTerminalEvidenceDebt {
                    route: publication.route,
                    record: publication.record,
                },
            );
        }
    }

    pub(crate) fn release_subscription_terminal_evidence(&self, reservation_id: &str) {
        lock(&self.subscription_terminal_evidence_publications).remove(reservation_id);
    }

    pub(crate) fn retry_subscription_terminal_evidence_debts(&self) {
        let debts = std::mem::take(&mut *lock(&self.subscription_terminal_evidence_debts));
        for debt in debts {
            self.publish_subscription_terminal_or_retain(debt.route, debt.record);
        }
    }

    /// After ingress quiesces, transfer only subscription terminals whose
    /// exact bytes reached the durable staging row. Failed staging retains its
    /// source authority and debt for an explicit retry; it is never released
    /// merely because shutdown was requested.
    fn handoff_subscription_terminal_evidence_debts_to_recovery(&self) -> Vec<DesktopError> {
        let debts = std::mem::take(&mut *lock(&self.subscription_terminal_evidence_debts));
        let mut retained = Vec::new();
        let mut failures = Vec::new();
        for debt in debts {
            let Some(scope) = debt.route.terminal_evidence_scope.as_ref() else {
                continue;
            };
            let outcome = (|| {
                let ledger = self.terminal_evidence_ledger().ok_or(
                    ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Unresolved,
                )?;
                let reservation = ledger.attach(&TerminalEvidenceScope {
                    producer: "subscription".to_owned(),
                    manager_id: scope.manager_id.clone(),
                    operation_id: scope.operation_id.clone(),
                    incarnation: scope.incarnation.clone(),
                    scope_utf16le: scope.scope_utf16le.clone(),
                })?;
                ledger.handoff_staged_to_recovery(&reservation.reservation_id)
            })();
            if let Err(error) = outcome {
                failures.push(terminal_recovery_error(error));
                retained.push(debt);
            }
        }
        if !retained.is_empty() {
            lock(&self.subscription_terminal_evidence_debts).extend(retained);
        }
        failures
    }

    fn mark_ended(&self, scope: &InstanceKey, route: &Route, terminal: StreamEnd) {
        if let Some(routes) = lock(&self.routes).get_mut(scope) {
            for entry in routes.iter_mut() {
                if entry.session_id == route.session_id && entry.consumer == route.consumer {
                    entry.terminal = Some(terminal.clone());
                }
            }
        }
    }

    pub(crate) fn recorded_route(
        &self,
        scope: &InstanceKey,
        session_id: u64,
        consumer: &str,
    ) -> Option<RecordedRoute> {
        lock(&self.routes).get(scope).and_then(|routes| {
            routes
                .iter()
                .find(|route| route.session_id == session_id && route.consumer == consumer)
                .and_then(|route| route.recorded.clone())
        })
    }

    /// Release a recorded consumer while preserving the central's final FIFO
    /// under the same per-route order as ordinary journal admission.
    pub(crate) async fn unsubscribe_recorded(
        &self,
        scope: &InstanceKey,
        session_id: u64,
        consumer: &str,
        core_consumer: &str,
        selector: &PathSelector,
        ctl: OpControl,
    ) -> Result<bool, DesktopError> {
        let settlement = ctl.settlement_guard();
        let recorded = self
            .recorded_route(scope, session_id, consumer)
            .ok_or_else(|| {
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                    ubm_core::contracts::BleErrorDomain::Gatt,
                    "gatt.unsubscribe.recorded-route",
                )
            })?;
        // A later FIFO slot must never hold the route or global ingress fence
        // while an earlier slot is waiting to acquire that same fence.
        let ctl = self
            .central
            .wait_unsubscribe_admission(&scope.0, ctl)
            .await?;
        let guard = recorded.order.clone().lock_owned().await;
        if recorded.radio_released.load(Ordering::SeqCst) {
            drop(guard);
            self.settle_recorded_ingress_drops().await?;
            self.remove_route(scope, session_id, consumer);
            return Ok(false);
        }
        let activation = self
            .central
            .subscription_epoch_for_consumer(
                &scope.0,
                &scope.1,
                scope.2,
                &scope.3,
                scope.4,
                core_consumer,
            )
            .await
            .ok_or_else(|| {
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                    ubm_core::contracts::BleErrorDomain::Gatt,
                    "gatt.unsubscribe.recorded-activation",
                )
            })?;
        let ingress_fence = self.radio.fence_ingress().await?;
        let central = self.central.clone();
        let peer = scope.0.clone();
        let selector = selector.clone();
        let core_consumer = core_consumer.to_owned();
        let consumer = consumer.to_owned();
        let route_consumer = consumer.clone();
        let delivery = lock(&self.routes)
            .get(scope)
            .and_then(|routes| {
                routes
                    .iter()
                    .find(|route| route.session_id == session_id && route.consumer == consumer)
                    .map(|route| route.delivery)
            })
            .unwrap_or("unknown");
        let runtime = self.runtime.clone();
        let (_disable_guard, _retired_ingress_fence) = match activation {
            ubm_desktop::central::ConsumerActivation::Active(epoch) => (
                Some(self.radio.stage_recorded_disable(
                    Instance {
                        peer_id: scope.0.clone(),
                        service_uuid: scope.1.clone(),
                        service_occurrence: scope.2,
                        characteristic_uuid: scope.3.clone(),
                        characteristic_occurrence: scope.4,
                    },
                    epoch,
                    ingress_fence,
                )?),
                None,
            ),
            ubm_desktop::central::ConsumerActivation::Retired => (None, Some(ingress_fence)),
        };
        let wake_session = self.session(session_id);
        let released_state = Arc::clone(&recorded.radio_released);
        let (result, guard) = tokio::task::spawn_blocking(move || {
            let _settlement = settlement;
            let _guard = guard;
            let _disable_guard = _disable_guard;
            let _retired_ingress_fence = _retired_ingress_fence;
            let drain = |poll| {
                use ubm_desktop::continuation_journal::RecordedLoss;
                let entry = match poll {
                    NotificationPoll::Value(bytes) => {
                        let impact = RecordedLoss { items: 1, bytes: Some(bytes.len() as u64) };
                        Some((serde_json::json!({"t":"value", "consumer":consumer,
                            "valueB64":wire::encode_base64(&bytes), "delivery":delivery}), impact))
                    }
                    NotificationPoll::ValueWithReceipt { value, receipt } => {
                        let impact = RecordedLoss { items: 1, bytes: Some(value.len() as u64) };
                        Some((serde_json::json!({"t":"value", "consumer":consumer,
                            "valueB64":wire::encode_base64(&value), "delivery":delivery, "receipt":receipt_value(receipt)}), impact))
                    }
                    NotificationPoll::ValueWithProvenance { value, receipt, provenance, source_generation, source_ordinal } => {
                        let impact = RecordedLoss { items: 1, bytes: Some(value.len() as u64) };
                        let mut record = serde_json::json!({"t":"value", "consumer":consumer,
                            "valueB64":wire::encode_base64(&value), "delivery":delivery, "provenance":provenance.as_str(), "sourceGeneration":source_generation.to_string(), "sourceOrdinal":source_ordinal.to_string()});
                        if let Some(receipt) = receipt { record["receipt"] = receipt_value(receipt); }
                        Some((record, impact))
                    }
                    NotificationPoll::PeerSourceLoss { peer_id, generation, skipped_records } => {
                        Some((serde_json::json!({"t":"peer-source-loss", "consumer":consumer,
                            "peerId":peer_id, "generation":generation.to_string(),
                            "skippedRecords":skipped_records.map(|count| count.to_string())}), RecordedLoss::default()))
                    }
                    NotificationPoll::SourceFailed(error) => {
                        Some((serde_json::json!({"t":"stream-end", "consumer":consumer,
                            "reason":"source-failed", "droppedItems":0, "droppedBytes":0, "error":wire::error_object(&error)}), RecordedLoss::default()))
                    }
                    NotificationPoll::SourceFailureRecovery(descriptor) => {
                        Some((
                            peer_source_failure_recovery_record(&consumer, &peer, &selector, descriptor),
                            RecordedLoss::default(),
                        ))
                    }
                    NotificationPoll::Terminal(terminal) => {
                        let impact = RecordedLoss { items: terminal.dropped_items(), bytes: Some(terminal.dropped_bytes()) };
                        Some((serde_json::json!({"t":"stream-end", "consumer":consumer,
                            "reason":"overflow", "droppedItems":terminal.dropped_items(), "droppedBytes":terminal.dropped_bytes()}), impact))
                    }
                    _ => None,
                };
                if let Some((record, impact)) = entry {
                    let _ = recorded.journal.append_recorded_batch(&[(&recorded.metadata, &record)], &[impact], RecordedLoss::default());
                    if let Some(session) = &wake_session { session.outbox.recording_changed(&consumer, &recorded.recording_id); }
                }
            };
            let result = runtime.block_on(central.unsubscribe_draining(&peer, &selector, &core_consumer, ctl, Some(&drain)));
            if result.is_ok() {
                released_state.store(true, Ordering::SeqCst);
            }
            (result, _guard)
        }).await.map_err(|_| DesktopError::new(ubm_core::contracts::BleErrorCode::PlatformFailure,
            ubm_core::contracts::BleErrorDomain::Platform, "gatt.unsubscribe.recorded-worker"))?;
        // The signal pump needs this same route-order lock to persist a queued
        // global drop. Retain the route, but release the lock before waiting.
        drop(guard);
        let released = result?;
        self.settle_recorded_ingress_drops().await?;
        self.remove_route(scope, session_id, &route_consumer);
        Ok(released)
    }

    /// One consumer's stream state: `Some(None)` live, `Some(Some(end))`
    /// ended, `None` when no route is installed.
    pub(crate) fn route_terminal(
        &self,
        scope: &InstanceKey,
        session_id: u64,
        consumer: &str,
    ) -> Option<Option<StreamEnd>> {
        lock(&self.routes).get(scope).and_then(|routes| {
            routes
                .iter()
                .find(|route| route.session_id == session_id && route.consumer == consumer)
                .map(|route| route.terminal.clone())
        })
    }

    /// One value turn for `scope`: commit at most one already-polled group of
    /// [`VALUE_RECORD_BATCH`] values, from the first route that holds any, and
    /// end streams on terminal answers. Routes without a value are searched
    /// without committing anything.
    ///
    /// Returns whether more work remains for the scope: the committing route
    /// may hold more values, or later routes were not visited. The caller
    /// requeues the scope so a queued security or lifecycle signal runs
    /// before the next commit.
    async fn flush_scope(&self, scope: &InstanceKey) -> bool {
        #[cfg(debug_assertions)]
        let counted = {
            let mut turns = lock(&self.scope_turns);
            let room = turns.len() < 1024;
            if room {
                turns.push(Vec::new());
            }
            room
        };
        let routes = self.live_routes(scope);
        for (index, route) in routes.iter().enumerate() {
            let unvisited = index + 1 < routes.len();
            let drain = self.drain_route(route, VALUE_RECORD_BATCH).await;
            let committed = drain.committed();
            #[cfg(debug_assertions)]
            if committed
                && counted
                && let Some(turn) = lock(&self.scope_turns).last_mut()
            {
                turn.push(route.consumer.clone());
            }
            match drain {
                // Nothing to commit: look at the next consumer.
                RouteDrain::Live { pending, .. } if !committed && !pending => {}
                // The committed route goes to the back so the next turn
                // starts at a consumer this one did not serve. Otherwise a
                // route with a standing backlog would be the first one
                // polled every turn, and later consumers of the scope would
                // never be polled while their core queues overflow.
                RouteDrain::Live { pending, .. } => {
                    self.rotate_route_to_end(scope, route);
                    return pending || unvisited;
                }
                RouteDrain::Ended {
                    terminal,
                    loss_accounted,
                    ..
                } => {
                    self.mark_ended(scope, route, terminal.clone());
                    self.end_route(route, terminal, loss_accounted);
                    if committed {
                        return unvisited;
                    }
                }
            }
        }
        false
    }

    /// Drain every value the core holds for the live routes of `scope`, in
    /// bounded commit groups, and return the routes that reached a terminal
    /// answer. The streams are marked ended and not emitted: the caller
    /// orders their terminals against lifecycle records.
    async fn drain_scope(&self, scope: &InstanceKey) -> Vec<(Route, StreamEnd, bool)> {
        let mut ended = Vec::new();
        for route in self.live_routes(scope) {
            if let RouteDrain::Ended {
                terminal,
                loss_accounted,
                ..
            } = self.drain_route(&route, usize::MAX).await
            {
                self.mark_ended(scope, &route, terminal.clone());
                ended.push((route, terminal, loss_accounted));
            }
        }
        ended
    }

    /// Move `route` behind the other routes of `scope`. The next bounded
    /// flush then starts at a consumer this turn did not finish.
    fn rotate_route_to_end(&self, scope: &InstanceKey, route: &Route) {
        let mut routes = lock(&self.routes);
        let Some(entries) = routes.get_mut(scope) else {
            return;
        };
        let Some(index) = entries.iter().position(|entry| {
            entry.session_id == route.session_id && entry.consumer == route.consumer
        }) else {
            return;
        };
        let deferred = entries.remove(index);
        entries.push(deferred);
    }

    fn live_routes(&self, scope: &InstanceKey) -> Vec<Route> {
        lock(&self.routes)
            .get(scope)
            .map(|routes| {
                routes
                    .iter()
                    .filter(|r| r.terminal.is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Move up to `limit` values the core holds for one consumer into its
    /// session's outbox, committing at most [`VALUE_RECORD_BATCH`] already
    /// polled values per group (so an unlimited lifecycle drain is a series
    /// of bounded commits). A full budget returns with work still pending and
    /// does not poll another value. A terminal answer is returned and not
    /// emitted; the caller orders it against lifecycle records. Lifecycle
    /// passes `usize::MAX` so values that arrived before the transition
    /// all land first.
    ///
    /// Every value polled before an empty/terminal/invalidated answer is
    /// committed first, in order. A refused group's values are counted once in
    /// the returned terminal: a rolled-back group is never reported accepted.
    async fn drain_route(&self, route: &Route, limit: usize) -> RouteDrain {
        let _recorded_order = match &route.recorded {
            Some(recorded) => Some(recorded.order.lock().await),
            None => None,
        };
        if route.recorded.is_some()
            && self
                .recorded_route(
                    &(
                        route.peer_id.clone(),
                        route.selector.service_uuid.clone(),
                        route.selector.service_occurrence.unwrap_or(0),
                        route
                            .selector
                            .characteristic_uuid
                            .clone()
                            .unwrap_or_default(),
                        route.selector.characteristic_occurrence.unwrap_or(0),
                    ),
                    route.session_id,
                    &route.consumer,
                )
                .is_none()
        {
            return RouteDrain::Live {
                committed: false,
                pending: false,
            };
        }
        #[cfg(debug_assertions)]
        {
            let mut turns = lock(&self.route_turns);
            if turns.len() < 1024 {
                turns.push(route.consumer.clone());
            }
        }
        let mut taken = 0usize;
        let mut committed = false;
        loop {
            if taken == limit {
                return RouteDrain::Live {
                    committed,
                    pending: true,
                };
            }
            let group = (limit - taken).min(VALUE_RECORD_BATCH);
            let mut records = Vec::with_capacity(group);
            let mut lengths = Vec::with_capacity(group);
            let mut session = None;
            let mut ending = PollEnd::Full;
            while records.len() < group {
                let poll = self
                    .central
                    .poll_notification(&route.peer_id, &route.selector, &route.core_consumer)
                    .await;
                match poll {
                    Ok(NotificationPoll::Value(bytes)) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        lengths.push(bytes.len());
                        records.push(object(vec![
                            ("t", Value::from("value")),
                            ("consumer", Value::from(route.consumer.as_str())),
                            ("valueB64", Value::from(wire::encode_base64(&bytes))),
                            ("delivery", Value::from(route.delivery)),
                        ]));
                    }
                    Ok(NotificationPoll::ValueWithReceipt {
                        value: bytes,
                        receipt,
                    }) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        lengths.push(bytes.len());
                        records.push(object(vec![
                            ("t", Value::from("value")),
                            ("consumer", Value::from(route.consumer.as_str())),
                            ("valueB64", Value::from(wire::encode_base64(&bytes))),
                            ("delivery", Value::from(route.delivery)),
                            ("receipt", receipt_value(receipt)),
                        ]));
                    }
                    Ok(NotificationPoll::ValueWithProvenance {
                        value: bytes,
                        receipt,
                        provenance,
                        source_generation,
                        source_ordinal,
                    }) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        lengths.push(bytes.len());
                        let mut record = object(vec![
                            ("t", Value::from("value")),
                            ("consumer", Value::from(route.consumer.as_str())),
                            ("valueB64", Value::from(wire::encode_base64(&bytes))),
                            ("delivery", Value::from(route.delivery)),
                            ("provenance", Value::from(provenance.as_str())),
                            (
                                "sourceGeneration",
                                Value::from(source_generation.to_string()),
                            ),
                            ("sourceOrdinal", Value::from(source_ordinal.to_string())),
                        ]);
                        if let Some(receipt) = receipt {
                            record["receipt"] = receipt_value(receipt);
                        }
                        records.push(record);
                    }
                    Ok(NotificationPoll::PeerSourceLoss {
                        peer_id,
                        generation,
                        skipped_records,
                    }) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        lengths.push(0);
                        records.push(
                            serde_json::json!({"t":"peer-source-loss", "consumer":route.consumer,
                            "peerId":peer_id, "generation":generation.to_string(),
                            "skippedRecords":skipped_records.map(|count| count.to_string())}),
                        );
                    }
                    Ok(NotificationPoll::Empty) => {
                        ending = PollEnd::Empty;
                        break;
                    }
                    Ok(NotificationPoll::Terminal(terminal)) => {
                        ending = PollEnd::Ended((
                            "overflow",
                            terminal.dropped_items(),
                            terminal.dropped_bytes(),
                            None,
                        ));
                        break;
                    }
                    Ok(NotificationPoll::Invalidated(_)) => {
                        ending = PollEnd::Ended(("invalidated", 0, 0, None));
                        break;
                    }
                    Ok(NotificationPoll::SourceFailed(error)) => {
                        ending = PollEnd::Ended(("source-failed", 0, 0, Some(error)));
                        break;
                    }
                    Ok(NotificationPoll::SourceFailureRecovery(descriptor)) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        lengths.push(0);
                        records.push(peer_source_failure_recovery_record(
                            &route.consumer,
                            &route.peer_id,
                            &route.selector,
                            descriptor,
                        ));
                        // The descriptor is delivered before the route's
                        // ordinary source-failed terminal. V1 claim views
                        // remove this V2-only control while retaining their
                        // legacy evidence projection.
                        ending = PollEnd::Ended(("source-failed", 0, 0, None));
                        break;
                    }
                    Ok(NotificationPoll::Closed) => {
                        ending = PollEnd::Ended(("closed", 0, 0, None));
                        break;
                    }
                    Err(error) => {
                        ending = PollEnd::Ended(("source-failed", 0, 0, Some(error)));
                        break;
                    }
                }
            }
            if let Some(session) = session {
                committed = true;
                if let Some(recorded) = &route.recorded {
                    let entries: Vec<(&Value, &Value)> = records
                        .iter()
                        .map(|record| (&recorded.metadata, record))
                        .collect();
                    let impacts: Vec<_> = records
                        .iter()
                        .zip(&lengths)
                        .map(
                            |(record, length)| ubm_desktop::continuation_journal::RecordedLoss {
                                items: u64::from(
                                    record.get("t").and_then(Value::as_str) == Some("value"),
                                ),
                                bytes: Some(*length as u64),
                            },
                        )
                        .collect();
                    let trailing = match &ending {
                        PollEnd::Ended((_, items, bytes, _)) => {
                            ubm_desktop::continuation_journal::RecordedLoss {
                                items: *items,
                                bytes: Some(*bytes),
                            }
                        }
                        PollEnd::Full | PollEnd::Empty => Default::default(),
                    };
                    let result = recorded
                        .journal
                        .append_recorded_batch(&entries, &impacts, trailing);
                    if !entries.is_empty()
                        || trailing.items > 0
                        || trailing.bytes.unwrap_or(0) > 0
                        || result.is_err()
                        || result.as_ref().is_ok_and(|batch| batch.rejected.is_some())
                    {
                        session
                            .outbox
                            .recording_changed(&route.consumer, &recorded.recording_id);
                    }
                    let (accepted, failure) = match result {
                        Ok(batch) => (batch.accepted, batch.rejected.map(|item| item.error)),
                        Err(failure) => (0, Some(failure)),
                    };
                    taken += accepted;
                    if failure.is_some() {
                        let refused = records
                            .iter()
                            .skip(accepted)
                            .filter(|record| record["t"] == "value")
                            .count() as u64;
                        let refused_bytes = lengths
                            .iter()
                            .skip(accepted)
                            .map(|length| *length as u64)
                            .sum::<u64>();
                        let (core_items, core_bytes) = match &ending {
                            PollEnd::Ended((_, items, bytes, _)) => (*items, *bytes),
                            PollEnd::Full | PollEnd::Empty => (0, 0),
                        };
                        return RouteDrain::Ended {
                            committed,
                            terminal: (
                                "closed",
                                refused.saturating_add(core_items),
                                refused_bytes.saturating_add(core_bytes),
                                None,
                            ),
                            loss_accounted: true,
                        };
                    }
                } else {
                    let outcome = session.outbox.push_data_batch(records);
                    taken += outcome.accepted;
                    if let Some(rejected) = outcome.rejected {
                        let core = match &ending {
                            PollEnd::Ended(terminal) => Some(terminal.clone()),
                            PollEnd::Full | PollEnd::Empty => None,
                        };
                        match refusal_terminal(&rejected, core) {
                            Some(terminal) => {
                                return RouteDrain::Ended {
                                    committed,
                                    terminal,
                                    loss_accounted: false,
                                };
                            }
                            // The stopped journal counted the refused group in its
                            // cutoff; nothing more is polled this turn unless the
                            // core already answered.
                            None if matches!(ending, PollEnd::Full) => {
                                return RouteDrain::Live {
                                    committed,
                                    pending: false,
                                };
                            }
                            None => {}
                        }
                    }
                }
            }
            match ending {
                PollEnd::Full => {}
                PollEnd::Empty => {
                    return RouteDrain::Live {
                        committed,
                        pending: false,
                    };
                }
                PollEnd::Ended(terminal) => {
                    return RouteDrain::Ended {
                        committed,
                        terminal,
                        loss_accounted: false,
                    };
                }
            }
        }
    }

    fn lifecycle_record(&self, event: &LifecycleEvent) -> Option<Value> {
        // A transition without a connection record cannot be matched to
        // any stream; the caller surfaces it instead of inventing one.
        let connection_generation = event.connection_generation.clone()?;
        let database_generation = crate::compat::lifecycle_database_generation(event);
        match event.kind {
            LifecycleKind::ServicesChanged => {
                let database_generation = database_generation?;
                // Kept for `session.reconcile` in case this record is lost.
                lock(&self.database_changes).insert(
                    event.peer_id.clone(),
                    (connection_generation.clone(), database_generation.clone()),
                );
                Some(object(vec![
                    ("t", Value::from("db-changed")),
                    ("peerId", Value::from(event.peer_id.as_str())),
                    ("connectionGeneration", Value::from(connection_generation)),
                    ("databaseGeneration", Value::from(database_generation)),
                ]))
            }
            LifecycleKind::LinkLost
            | LifecycleKind::HostReset
            | LifecycleKind::AdapterLost
            | LifecycleKind::Released { .. } => {
                let reason = match event.kind {
                    LifecycleKind::Released { requested: true } => "local",
                    // The core ended the link because the adapter went away.
                    LifecycleKind::AdapterLost => "adapter",
                    LifecycleKind::HostReset => "backend-restart",
                    _ => {
                        let adapter_down = lock(&self.adapter)
                            .as_ref()
                            .is_some_and(|(snapshot, _)| snapshot.power != AdapterPower::On);
                        if adapter_down { "adapter" } else { "peer" }
                    }
                };
                // Kept for `session.reconcile` in case this record is lost.
                lock(&self.link_ends).insert(
                    event.peer_id.clone(),
                    LinkEnd {
                        connection_generation: connection_generation.clone(),
                        database_generation: database_generation.clone(),
                        reason,
                    },
                );
                let mut record = object(vec![
                    ("t", Value::from("link")),
                    ("peerId", Value::from(event.peer_id.as_str())),
                    ("connectionGeneration", Value::from(connection_generation)),
                    (
                        "databaseGeneration",
                        opt_text(database_generation.as_deref()),
                    ),
                    ("reason", Value::from(reason)),
                ]);
                if event.kind == LifecycleKind::HostReset {
                    record["platform"] = serde_json::json!({"domain":"ubm", "code":"source-observation-overflow", "message":"Host observation capacity exceeded", "metadata":{}});
                }
                Some(record)
            }
        }
    }

    /// Order per peer: values that arrived before the transition, then the
    /// transition record, then the stream ends it caused.
    async fn route_lifecycle(&self, event: LifecycleEvent) {
        self.reconcile_security_lifecycle(&event);
        let scopes: Vec<InstanceKey> = lock(&self.routes)
            .keys()
            .filter(|scope| scope.0 == event.peer_id)
            .cloned()
            .collect();
        let mut ended = Vec::new();
        for scope in &scopes {
            for (route, terminal, loss_accounted) in self.drain_scope(scope).await {
                let terminal = if event.kind == LifecycleKind::HostReset {
                    let replacement = ("source-failed", 0, 0, Some(host_reset_error()));
                    self.mark_ended(scope, &route, replacement.clone());
                    replacement
                } else {
                    terminal
                };
                ended.push((route, terminal, loss_accounted));
            }
        }
        match self.lifecycle_record(&event) {
            Some(record) => self.broadcast(&record),
            None => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop(IngressClass::Control);
                }
            }
        }
        for (route, terminal, loss_accounted) in ended {
            self.end_route(&route, terminal, loss_accounted);
        }
        // Hubs the core invalidated after the first pass end here.
        for scope in &scopes {
            for (route, terminal, loss_accounted) in self.drain_scope(scope).await {
                let terminal = if event.kind == LifecycleKind::HostReset {
                    let replacement = ("source-failed", 0, 0, Some(host_reset_error()));
                    self.mark_ended(scope, &route, replacement.clone());
                    replacement
                } else {
                    terminal
                };
                self.end_route(&route, terminal, loss_accounted);
            }
            if event.kind == LifecycleKind::HostReset {
                for route in self.live_routes(scope) {
                    let terminal = ("source-failed", 0, 0, Some(host_reset_error()));
                    self.mark_ended(scope, &route, terminal.clone());
                    self.end_route(&route, terminal, false);
                }
            }
        }
    }

    fn reconcile_security_lifecycle(&self, event: &LifecycleEvent) {
        if !matches!(
            event.kind,
            ubm_desktop::LifecycleKind::LinkLost
                | ubm_desktop::LifecycleKind::Released { .. }
                | ubm_desktop::LifecycleKind::AdapterLost
                | ubm_desktop::LifecycleKind::HostReset
        ) {
            return;
        }
        let Some(generation) = event.connection_generation.as_deref() else {
            return;
        };
        let Some(state) = (|| {
            let mut security = lock(&self.security);
            let entry = security.get_mut(&event.peer_id)?;
            reset_security_for_generation(entry, generation)
        })() else {
            return;
        };
        self.signals
            .push(HostSignal::Security(event.peer_id.clone(), state));
    }

    pub(crate) fn stamp_security_generation(&self, peer_id: &str, generation: &str) -> bool {
        if generation.is_empty() {
            return false;
        }
        let mut security = lock(&self.security);
        let entry = security.entry(peer_id.to_owned()).or_insert_with(|| {
            SecurityEntry::new(SecurityState {
                bond: BondState::Unknown,
                encryption: crate::radio::EncryptionState::Unknown,
                authentication: crate::radio::AuthenticationState::Unknown,
                secure_connections: crate::radio::SecureConnectionsState::Unknown,
                pairing_possible: None,
            })
        });
        if entry.owner_generation.as_deref() == Some(generation) {
            return false;
        }
        if entry.state.encryption != crate::radio::EncryptionState::Unsupported {
            entry.state.encryption = crate::radio::EncryptionState::Unknown;
        }
        if entry.state.authentication != crate::radio::AuthenticationState::Unsupported {
            entry.state.authentication = crate::radio::AuthenticationState::Unknown;
        }
        if entry.state.secure_connections != crate::radio::SecureConnectionsState::Unsupported {
            entry.state.secure_connections = crate::radio::SecureConnectionsState::Unknown;
        }
        entry.owner_generation = Some(generation.to_owned());
        true
    }

    pub(crate) fn security_owner(&self, peer_id: &str) -> Option<String> {
        lock(&self.security)
            .get(peer_id)
            .and_then(|entry| entry.owner_generation.clone())
    }

    pub(crate) fn set_security_state_if_owner(
        &self,
        peer_id: &str,
        expected_owner: Option<&str>,
        state: SecurityState,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        let mut security = lock(&self.security);
        let current_owner = security
            .get(peer_id)
            .and_then(|entry| entry.owner_generation.as_deref());
        if current_owner != expected_owner {
            return Err(DesktopError::new(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                operation,
            ));
        }
        let owner_generation = current_owner.map(str::to_owned);
        security.insert(
            peer_id.to_owned(),
            SecurityEntry {
                state,
                owner_generation,
            },
        );
        Ok(())
    }

    pub(crate) fn clear_security_generation(&self, peer_id: &str, generation: &str) {
        let mut security = lock(&self.security);
        if let Some(entry) = security.get_mut(peer_id)
            && entry.owner_generation.as_deref() == Some(generation)
        {
            entry.owner_generation = None;
        }
    }

    /// Legacy order after an adapter loss (`releaseCoreBluetoothAdapterLossResources`,
    /// then `advanceGeneration`): the scan members end `source-failed` (the
    /// core already stopped and ended the owned scan), then the new
    /// generations are published as an `adapter` record over the last
    /// platform snapshot. Links and streams ended through their own
    /// lifecycle records before this signal.
    async fn adapter_reset(
        &self,
        attachment: &AttachmentTuple,
        ended_scan: Option<OperationId>,
        cause: ubm_desktop::AdapterLossCause,
        resources: Vec<(Arc<SessionState>, crate::session::ResetResources)>,
    ) {
        let scan = if let Some(ended_scan) = ended_scan {
            let mut share = self.scan.lock().await;
            // Reset reports the core operation it actually ended. A later
            // scan can already own the shared slot by the time this signal
            // reaches the pump, and must not inherit the old terminal.
            if !share
                .physical
                .as_ref()
                .is_some_and(|scan| scan.operation == ended_scan)
            {
                drop(share);
                None
            } else {
                let mut ingress = lock(&self.scan_ingress);
                if ingress
                    .queue
                    .iter()
                    .any(|item| matches!(item, ScanIngressItem::FailureCut(_)))
                {
                    // A captured radio terminal owns that cut. Retry this reset
                    // after its queued signal rather than crossing the cut.
                    drop(ingress);
                    drop(share);
                    self.signals.push(HostSignal::AdapterReset(
                        attachment.clone(),
                        Some(ended_scan),
                        cause,
                        resources,
                    ));
                    return;
                }
                let members = self.take_scan_members();
                for (id, _) in &members {
                    *ingress.pending_by_session.entry(*id).or_default() += 1;
                }
                let generation = ingress.generation.take();
                share.physical = None;
                let prefix: Vec<_> = ingress
                    .queue
                    .drain(..)
                    .map(|item| match item {
                        ScanIngressItem::Advertisement(record) => record,
                        ScanIngressItem::FailureCut(_) => unreachable!("failure cuts were checked"),
                    })
                    .collect();
                Some((members, generation, prefix))
            }
        } else {
            None
        };
        for (session, captured) in resources {
            for (scope, consumer) in session.end_by_reset(captured) {
                self.remove_route(&scope, session.id, &consumer);
            }
        }
        if let Some((members, generation, prefix)) = scan {
            for record in prefix {
                self.route_advertisement(record);
            }
            let recipients: Vec<_> = members.iter().map(|(id, _)| *id).collect();
            if let Some(generation) = generation {
                let code = match cause {
                    ubm_desktop::AdapterLossCause::PoweredOff => BleErrorCode::AdapterPoweredOff,
                    ubm_desktop::AdapterLossCause::Resetting => BleErrorCode::AdapterResetting,
                    _ => BleErrorCode::AdapterUnavailable,
                };
                let error = DesktopError::new(code, BleErrorDomain::Adapter, "scan.start")
                    .with_detail(cause.as_str());
                self.end_failed_scan_members(
                    members,
                    generation,
                    "adapter",
                    &error,
                    &HashSet::new(),
                );
            } else {
                self.end_scan_members(members, "source-failed");
            }
            for id in recipients {
                self.finish_scan_ingress(id);
            }
        }
        let last = lock(&self.adapter).clone();
        match last {
            Some((snapshot, updated_at)) => {
                let record = object(vec![
                    ("t", Value::from("adapter")),
                    ("state", adapter_value(&snapshot, updated_at, attachment)),
                ]);
                self.broadcast(&record);
            }
            // A reset needs a lost adapter, which only a platform snapshot
            // reports; without one the advance cannot be published.
            None => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop(IngressClass::Control);
                }
            }
        }
    }

    fn take_scan_members(&self) -> Vec<(u64, ScanMember)> {
        std::mem::take(&mut *lock(&self.scan_members))
            .into_iter()
            .collect()
    }

    fn end_scan_members(&self, members: Vec<(u64, ScanMember)>, reason: &'static str) {
        for (session_id, member) in members {
            member.expiry_cancel.request_cancel();
            if let Some(session) = self.scan_session(session_id) {
                session.clear_scan(&member.membership);
                if reason == "source-failed" && *lock(&session.scan_end_version) == Some(2) {
                    session.outbox.push_ingress_drop(IngressClass::Control);
                    continue;
                }
                let mut record = object(vec![
                    ("t", Value::from("scan-end")),
                    ("operationId", Value::from(member.membership.as_str())),
                    ("reason", Value::from(reason)),
                ]);
                self.publish_scan_terminal_or_retain(session_id, member, record);
            }
        }
    }

    fn end_failed_scan_members(
        &self,
        members: Vec<(u64, ScanMember)>,
        generation: RequestId,
        source: &str,
        error: &DesktopError,
        retained: &HashSet<u64>,
    ) {
        for (session_id, member) in members {
            member.expiry_cancel.request_cancel();
            if let Some(session) = self.scan_session(session_id) {
                session.clear_scan(&member.membership);
                if retained.contains(&session_id) {
                    continue;
                }
                let mut record = if *lock(&session.scan_end_version) == Some(2) {
                    let mut cause = wire::error_object(error);
                    cause.insert(
                        "retryability".into(),
                        Value::from(error.retryability().as_str()),
                    );
                    serde_json::json!({"t":"scan-end", "operationId":member.membership,
                        "startOperationId":member.start_operation_id, "reason":"source-failed",
                        "version":2, "source":source, "generation":generation.to_string(),
                        "error":cause})
                } else {
                    serde_json::json!({"t":"scan-end", "operationId":member.membership,"reason":"source-failed"})
                };
                self.publish_scan_terminal_or_retain(session_id, member, record);
            }
        }
    }

    /// Arm a native membership clock. No JavaScript timer, event drain or
    /// runtime activity is required to stop delivering after its deadline.
    pub(crate) fn arm_scan_deadline(self: &Arc<Self>, session_id: u64, membership: &str) {
        let timer = lock(&self.scan_members)
            .get(&session_id)
            .and_then(|member| {
                (member.membership == membership)
                    .then_some(member)
                    .and_then(|member| {
                        member
                            .deadline
                            .map(|deadline| (deadline, member.expiry_cancel.clone()))
                    })
            });
        let Some((deadline, cancel)) = timer else {
            return;
        };
        let owner = Arc::downgrade(self);
        self.runtime.spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = tokio::time::sleep_until(deadline) => {
                    if let Some(owner) = owner.upgrade() {
                        owner.signals.push(HostSignal::ScanDeadlines);
                    }
                }
            }
        });
    }

    /// Runs on the same ordered pump as advertisement delivery: a cloned
    /// advertisement membership cannot publish behind its terminal. Native
    /// cleanup is separate so a slow scan stop cannot stall connected data.
    fn expire_scan_members(self: &Arc<Self>) {
        let now = tokio::time::Instant::now();
        let (expired, prefix, remaining) = {
            let mut ingress = lock(&self.scan_ingress);
            if ingress
                .queue
                .iter()
                .any(|item| matches!(item, ScanIngressItem::FailureCut(_)))
            {
                if !ingress.deferred_failures.is_empty() {
                    // The held start, rather than another pump turn, must
                    // wake this retry. Requeueing here would spin the pump.
                    ingress.deferred_deadlines = true;
                    return;
                }
                // The failure signal owns its cut and precedes this retry.
                self.signals.push(HostSignal::ScanDeadlines);
                return;
            }
            let mut members = lock(&self.scan_members);
            let ids: Vec<_> = members
                .iter()
                .filter_map(|(id, member)| {
                    member
                        .deadline
                        .is_some_and(|deadline| now >= deadline)
                        .then_some(*id)
                })
                .collect();
            let expired = ids
                .into_iter()
                .filter_map(|id| members.remove(&id).map(|member| (id, member)))
                .collect::<Vec<_>>();
            for (id, _) in &expired {
                *ingress.pending_by_session.entry(*id).or_default() += 1;
            }
            let identities: HashSet<_> = expired
                .iter()
                .map(|(id, member)| (*id, member.membership.clone()))
                .collect();
            let mut prefix = Vec::new();
            let queued = std::mem::take(&mut ingress.queue);
            for item in queued {
                if let ScanIngressItem::Advertisement(mut record) = item {
                    let mut retired = Vec::new();
                    record.members.retain(|(id, member)| {
                        if identities.contains(&(*id, member.membership.clone())) {
                            retired.push((*id, member.clone()));
                            false
                        } else {
                            true
                        }
                    });
                    if !retired.is_empty() {
                        prefix.push(CapturedAdvertisement {
                            generation: record.generation,
                            snapshot: record.snapshot.clone(),
                            members: retired,
                            observed_at: record.observed_at,
                        });
                        if record.members.is_empty() {
                            continue;
                        }
                    }
                    ingress
                        .queue
                        .push_back(ScanIngressItem::Advertisement(record));
                } else {
                    ingress.queue.push_back(item);
                }
            }
            let remaining = !ingress.queue.is_empty();
            (expired, prefix, remaining)
        };
        for record in prefix {
            self.route_advertisement(record);
        }
        if remaining {
            self.signals.push(HostSignal::Advertisements);
        }
        if expired.is_empty() {
            return;
        }
        let recipients: Vec<_> = expired.iter().map(|(id, _)| *id).collect();
        self.end_scan_members(expired, "operation-timed-out");
        for id in recipients {
            self.finish_scan_ingress(id);
        }
        let owner = Arc::clone(self);
        self.runtime.spawn(async move {
            let mut share = owner.scan.lock().await;
            // Another live member, including one admitted while we waited,
            // owns the physical scan. Never stop it for a retired generation.
            if !lock(&owner.scan_members).is_empty() {
                return;
            }
            let Some(operation) = share.physical.as_ref().map(|scan| scan.operation.clone()) else {
                return;
            };
            match owner
                .central
                .stop_scan(&operation, OpControl::unbounded())
                .await
            {
                Ok(_) => {
                    share.physical = None;
                    lock(&owner.scan_ingress).generation = None;
                }
                Err(error) => {
                    eprintln!(
                        "ubm-mobile: expired scan cleanup retained by process owner: {error}"
                    );
                    if !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        owner.runtime.spawn(Arc::clone(&owner).retry_orphan_scan());
                    }
                }
            }
        });
    }

    async fn scan_failed(
        self: &Arc<Self>,
        error: DesktopError,
        expected: Option<RequestId>,
        captured: Vec<(u64, ScanMember)>,
        source: &'static str,
        retained: &HashSet<u64>,
    ) {
        if let Some(generation) = expected {
            // Failure owns its callback-time recipients independently of a
            // stop in flight. Do not hold their publication behind radio I/O.
            {
                let _ingress = lock(&self.scan_ingress);
                let mut members = lock(&self.scan_members);
                for (id, captured_member) in &captured {
                    if members
                        .get(id)
                        .is_some_and(|current| current.membership == captured_member.membership)
                    {
                        members.remove(id);
                    }
                }
            }
            let recipients: Vec<_> = captured.iter().map(|(id, _)| *id).collect();
            self.end_failed_scan_members(captured, generation, source, &error, retained);
            for id in recipients {
                self.finish_scan_ingress(id);
            }
            let owner = Arc::clone(self);
            self.runtime.spawn(async move {
                let mut share = owner.scan.lock().await;
                let current = lock(&owner.scan_ingress).generation;
                if current.is_some_and(|current| current != generation)
                    || !lock(&owner.scan_members).is_empty()
                {
                    return;
                }
                if let Some(operation) = share.physical.as_ref().map(|scan| scan.operation.clone())
                {
                    match owner
                        .central
                        .stop_scan(&operation, OpControl::unbounded())
                        .await
                    {
                        Ok(_) => share.physical = None,
                        Err(error) => {
                            eprintln!(
                                "ubm-mobile: failed scan cleanup retained by process owner: {error}"
                            );
                            if !share.orphan_retry_scheduled {
                                share.orphan_retry_scheduled = true;
                                owner.runtime.spawn(Arc::clone(&owner).retry_orphan_scan());
                            }
                            return;
                        }
                    }
                }
                let mut ingress = lock(&owner.scan_ingress);
                if ingress.generation == Some(generation) {
                    ingress.generation = None;
                }
            });
            return;
        }
        let recipients: Vec<_> = if expected.is_some() {
            captured.iter().map(|(id, _)| *id).collect()
        } else {
            Vec::new()
        };
        let share = self.scan.lock().await;
        if expected.is_some_and(|expected| lock(&self.scan_ingress).generation != Some(expected)) {
            drop(share);
            if let Some(generation) = expected {
                let mut members = lock(&self.scan_members);
                for (id, captured_member) in &captured {
                    if members
                        .get(id)
                        .is_some_and(|current| current.membership == captured_member.membership)
                    {
                        members.remove(id);
                    }
                }
                drop(members);
                self.end_failed_scan_members(captured, generation, source, &error, retained);
            }
            for id in recipients {
                self.finish_scan_ingress(id);
            }
            return;
        }
        let generation = lock(&self.scan_ingress).generation;
        lock(&self.scan_ingress).generation = None;
        let members = self.take_scan_members();
        // The callback's terminal follows its captured prefix; native stop is
        // cleanup and must not delay publishing the source's answer.
        if let Some(generation) = generation {
            self.end_failed_scan_members(
                if expected.is_some() {
                    captured
                } else {
                    members
                },
                generation,
                "radio",
                &error,
                retained,
            );
        } else {
            self.end_scan_members(members, "source-failed");
        }
        for id in recipients {
            self.finish_scan_ingress(id);
        }
        let operation = share.physical.as_ref().map(|scan| scan.operation.clone());
        drop(share);
        if let Some(operation) = operation {
            let owner = Arc::clone(self);
            self.runtime.spawn(async move {
                let mut share = owner.scan.lock().await;
                if !share
                    .physical
                    .as_ref()
                    .is_some_and(|scan| scan.operation == operation)
                    || !lock(&owner.scan_members).is_empty()
                {
                    return;
                }
                // Keep the exact central operation until the platform confirms
                // cleanup. A failed native stop may still own its callback.
                match owner
                    .central
                    .stop_scan(&operation, OpControl::unbounded())
                    .await
                {
                    Ok(_) => share.physical = None,
                    Err(error) => {
                        eprintln!(
                            "ubm-mobile: failed scan cleanup retained by process owner: {error}"
                        );
                        if !share.orphan_retry_scheduled {
                            share.orphan_retry_scheduled = true;
                            owner.runtime.spawn(Arc::clone(&owner).retry_orphan_scan());
                        }
                    }
                }
            });
        }
    }

    /// The op is dead: cancelled, or past its caller budget. Cancellation
    /// reports `operation.aborted`; an expired budget reports
    /// `operation.timed-out` with no backstop detail (the caller set it).
    fn scan_not_alive(ctl: &OpControl) -> Option<DesktopError> {
        Self::scan_not_alive_parts(&ctl.ticket, &ctl.budget)
    }

    fn scan_not_alive_parts(ticket: &OpTicket, budget: &Budget) -> Option<DesktopError> {
        if ticket.is_cancel_requested() {
            return Some(DesktopError::new(
                BleErrorCode::OperationAborted,
                BleErrorDomain::Scan,
                "scan.start",
            ));
        }
        if budget.is_expired() {
            return Some(DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Scan,
                "scan.start",
            ));
        }
        None
    }

    /// Acquire the shared-scan admission section in a cancellation- and
    /// deadline-aware way (X-R1): an op cancelled or expired while queued on
    /// the scan mutex never takes a membership. The checks run again inside
    /// the section, so the no-radio fast path cannot admit a dead op
    /// either. This is deliberately not a timeout around the whole
    /// mutation: abandoning the join after the widen restart stopped the
    /// previous physical scan would orphan every member.
    async fn lock_scan_share(
        &self,
        ctl: &OpControl,
    ) -> Result<tokio::sync::MutexGuard<'_, ScanShare>, DesktopError> {
        if let Some(error) = Self::scan_not_alive(ctl) {
            return Err(error);
        }
        let guard = match ctl.budget.remaining() {
            Some(wait) => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => return Err(Self::scan_not_alive(ctl)
                        .unwrap_or_else(|| DesktopError::new(
                            BleErrorCode::OperationAborted,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ))),
                    () = tokio::time::sleep(wait) => return Err(DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )),
                    guard = self.scan.lock() => guard,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => return Err(Self::scan_not_alive(ctl)
                        .unwrap_or_else(|| DesktopError::new(
                            BleErrorCode::OperationAborted,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ))),
                    guard = self.scan.lock() => guard,
                }
            }
        };
        if let Some(error) = Self::scan_not_alive(ctl) {
            return Err(error);
        }
        Ok(guard)
    }

    /// Start, join or widen the shared physical scan for one member.
    pub(crate) async fn join_scan(
        self: &Arc<Self>,
        session_id: u64,
        member: ScanMember,
        android: Option<AndroidScanOptions>,
        ctl: OpControl,
        orphan_cleanup: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(), DesktopError> {
        let mut share = self.lock_scan_share(&ctl).await?;
        let mut wanted = {
            let members = lock(&self.scan_members);
            let everyone: Vec<&ScanMember> = members
                .iter()
                .filter(|(id, _)| **id != session_id)
                .map(|(_, member)| member)
                .chain(std::iter::once(&member))
                .collect();
            ScanRequest {
                service_uuids: union_or_broad(everyone.iter().map(|m| &m.service_uuids)),
                device_addresses: union_or_broad(everyone.iter().map(|m| &m.device_addresses)),
                android,
            }
        };
        // A retained physical record with no members is cleanup debt, not
        // a shareable scan. A new manager must stop that exact generation
        // before it can acquire a fresh radio scan. The orphan worker uses
        // this same lock, so it cannot race a replacement into stopping the
        // new generation. A refused cleanup leaves the old record and retry
        // driver intact.
        if lock(&self.scan_members).is_empty()
            && let Some(operation) = share.physical.as_ref().map(|scan| scan.operation.clone())
        {
            // Notify the admission waiter only after the locked state check.
            // A preflight snapshot would race another caller creating this
            // orphan before we acquire the scan section.
            let _ = orphan_cleanup.send(());
            self.central
                .stop_scan(
                    &operation,
                    OpControl::new(ctl.budget, ubm_desktop::OpTicket::new()),
                )
                .await?;
            share.physical = None;
            lock(&self.scan_ingress).generation = None;
            if let Some(error) = Self::scan_not_alive(&ctl) {
                return Err(error);
            }
        }
        let mut next_generation = None;
        if let Some(physical) = &share.physical {
            {
                let capture = lock(&self.scan_ingress);
                if capture.failed_generation == capture.generation && capture.generation.is_some() {
                    return Err(DesktopError::new(
                        BleErrorCode::ScanStartFailed,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )
                    .with_detail("physical scan has reported a source failure"));
                }
            }
            if physical.request.android != android {
                return Err(DesktopError::new(
                    BleErrorCode::ScanAlreadyActive,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail("the shared scan runs with different platform options"));
            }
            if covers(&physical.request.service_uuids, &member.service_uuids)
                && covers(&physical.request.device_addresses, &member.device_addresses)
            {
                // No radio call below would notice a dead op: re-check the
                // ticket and the deadline before taking the membership.
                if let Some(error) = Self::scan_not_alive(&ctl) {
                    return Err(error);
                }
                let capture = lock(&self.scan_ingress);
                if capture.failed_generation == capture.generation && capture.generation.is_some() {
                    return Err(DesktopError::new(
                        BleErrorCode::ScanStartFailed,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )
                    .with_detail("physical scan has reported a source failure"));
                }
                lock(&self.scan_members).insert(session_id, member);
                return Ok(());
            }
            // Widen: restart the physical scan with the union filter.
            // Reserve its identity before stopping the scan serving the old
            // members, so exhaustion cannot strand their live membership.
            next_generation = Some(ForeignRadio::next_request_id()?);
            let operation = physical.operation.clone();
            // The restart's stop runs under the caller's budget with its own
            // ticket: one ticket settles once, and the start below is the
            // operation the caller's cancel targets.
            self.central
                .stop_scan(
                    &operation,
                    OpControl::new(ctl.budget, ubm_desktop::OpTicket::new()),
                )
                .await?;
            share.physical = None;
            {
                let mut ingress = lock(&self.scan_ingress);
                if ingress.failed_generation == ingress.generation && ingress.generation.is_some() {
                    // The old source failed while stop was in flight. Leave its
                    // identity published for the queued failure to retire.
                    return Err(DesktopError::new(
                        BleErrorCode::ScanStartFailed,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )
                    .with_detail("physical scan failed during replacement"));
                }
                ingress.generation = None;
            }
        }
        sort_dedup(&mut wanted.service_uuids);
        sort_dedup(&mut wanted.device_addresses);
        let generation = match next_generation {
            Some(generation) => generation,
            None => ForeignRadio::next_request_id()?,
        };
        {
            let mut ingress = lock(&self.scan_ingress);
            ingress.generation = Some(generation);
            ingress.attachment = Some(self.central.attachment());
            ingress.operation = None;
            ingress.failed_generation = None;
            ingress.pending = Some((session_id, member.clone()));
        }
        let _start_capture = ScanStartCapture {
            host: self,
            generation,
        };
        self.radio.stage_scan(generation, wanted.clone());
        let filter: Vec<&str> = wanted.service_uuids.iter().map(String::as_str).collect();
        // `ctl` moves into the start call; keep the liveness witnesses for
        // the post-call membership check below.
        let ticket = ctl.ticket.clone();
        let budget = ctl.budget;
        let started = self
            .central
            .start_scan(&format!("ubm-mobile-scan-{session_id}"), &filter, ctl)
            .await;
        self.radio.clear_staging(None, None, true);
        #[cfg(test)]
        if let Some((entered, release)) = { lock(&self.after_scan_start).take() } {
            entered.notify_one();
            release.notified().await;
        }
        match started {
            Ok(session) => {
                let operation = session.operation_id().clone();
                {
                    let mut ingress = lock(&self.scan_ingress);
                    if ingress.generation == Some(generation) {
                        ingress.operation = Some(operation.clone());
                    }
                }
                share.physical = Some(PhysicalScan {
                    operation: operation.clone(),
                    request: wanted,
                });
                // The radio call took a while: a queued cancel or an
                // expired budget must not take a membership for a dead op.
                if let Some(error) = Self::scan_not_alive_parts(&ticket, &budget) {
                    lock(&self.scan_ingress).pending = None;
                    self.signals.push(HostSignal::Advertisements);
                    let cleanup = self
                        .central
                        .stop_scan(&operation, OpControl::unbounded())
                        .await;
                    if cleanup.is_ok() {
                        // Widening already stopped the scan that served the
                        // existing members. The replacement is gone too, so
                        // those members must be told that their source ended.
                        share.physical = None;
                        {
                            let mut ingress = lock(&self.scan_ingress);
                            if ingress.failed_generation != Some(generation)
                                && !lock(&self.scan_members).is_empty()
                                && self
                                    .capture_scan_failure_locked(
                                        &mut ingress,
                                        error.clone(),
                                        Some(generation),
                                        "radio",
                                    )
                                    .is_err()
                            {
                                eprintln!(
                                    "ubm-mobile: could not capture cancelled replacement scan terminal"
                                );
                            }
                        }
                        drop(share);
                        return Err(error);
                    }
                    // A refused cleanup stays in both owners. Existing
                    // members can retry their ordinary stop; a first scanner
                    // has no member, so the process owner drives that debt.
                    if lock(&self.scan_members).is_empty() && !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        self.runtime.spawn(Arc::clone(self).retry_orphan_scan());
                    }
                    return Err(error);
                }
                {
                    let mut ingress = lock(&self.scan_ingress);
                    lock(&self.scan_members).insert(session_id, member);
                    ingress.pending = None;
                }
                self.signals.push(HostSignal::Advertisements);
                Ok(())
            }
            Err(error) => {
                {
                    let mut ingress = lock(&self.scan_ingress);
                    ingress.pending = None;
                    let failed = ingress.failed_generation == Some(generation);
                    let has_members = !lock(&self.scan_members).is_empty();
                    if !has_members && !failed {
                        ingress.generation = None;
                    }
                    if !failed
                        && has_members
                        && self
                            .capture_scan_failure_locked(
                                &mut ingress,
                                error.clone(),
                                Some(generation),
                                "radio",
                            )
                            .is_err()
                    {
                        eprintln!("ubm-mobile: could not capture failed replacement scan terminal");
                    }
                }
                self.signals.push(HostSignal::Advertisements);
                // A failed restart leaves the previous members without a
                // radio scan: end their streams instead of pretending. The
                // central can nevertheless retain this generation after a
                // failed compensating stop. There is no public membership
                // left to retry it, so transfer that exact identity to the
                // process-owned orphan driver before returning the error.
                if let Some(operation) = self.central.active_scan_id() {
                    share.physical = Some(PhysicalScan {
                        operation,
                        request: wanted,
                    });
                    if !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        self.runtime.spawn(Arc::clone(self).retry_orphan_scan());
                    }
                }
                drop(share);
                Err(error)
            }
        }
    }

    async fn retry_orphan_scan(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if self.shut_down.load(Ordering::SeqCst) {
                return;
            }
            let mut share = self.scan.lock().await;
            if !lock(&self.scan_members).is_empty() || share.physical.is_none() {
                share.orphan_retry_scheduled = false;
                return;
            }
            let operation = share.physical.as_ref().unwrap().operation.clone();
            match self
                .central
                .stop_scan(&operation, OpControl::unbounded())
                .await
            {
                Ok(_) => {
                    share.physical = None;
                    lock(&self.scan_ingress).generation = None;
                    share.orphan_retry_scheduled = false;
                    return;
                }
                Err(error) => {
                    eprintln!("ubm-mobile: retained orphan scan cleanup failed: {error}");
                }
            }
        }
    }

    /// Leave the shared scan; the last member stops the physical scan. A
    /// failed stop keeps the membership so the caller can retry.
    pub(crate) async fn leave_scan(
        &self,
        session_id: u64,
        membership: &str,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let mut share = self.scan.lock().await;
        let last = {
            let members = lock(&self.scan_members);
            if !members
                .get(&session_id)
                .is_some_and(|member| member.membership == membership)
            {
                return Ok(());
            }
            members.len() == 1
        };
        if last && let Some(physical) = &share.physical {
            let operation = physical.operation.clone();
            self.central.stop_scan(&operation, ctl).await?;
            share.physical = None;
            lock(&self.scan_ingress).generation = None;
        }
        let removed = {
            let mut members = lock(&self.scan_members);
            if members
                .get(&session_id)
                .is_some_and(|member| member.membership == membership)
            {
                members.remove(&session_id)
            } else {
                None
            }
        };
        if let Some(member) = removed {
            member.expiry_cancel.request_cancel();
        }
        Ok(())
    }

    pub(crate) fn register_pending_recording<'a>(
        &'a self,
        session_id: u64,
        consumer: &'a str,
        route: RecordedRoute,
    ) -> PendingRecordedRoute<'a> {
        lock(&self.pending_recordings).insert((session_id, consumer.to_owned()), route);
        PendingRecordedRoute {
            host: self,
            session_id,
            consumer,
        }
    }

    pub(crate) fn add_route(&self, scope: InstanceKey, route: Route) {
        let mut routes = lock(&self.routes);
        let mut pending = lock(&self.pending_recordings);
        pending.remove(&(route.session_id, route.consumer.clone()));
        let entries = routes.entry(scope).or_default();
        entries.retain(|entry| {
            !(entry.session_id == route.session_id && entry.consumer == route.consumer)
        });
        entries.push(route);
    }

    fn notify_recorded_change(&self, session_id: u64, consumer: &str, recorded: &RecordedRoute) {
        let routes = lock(&self.routes);
        let pending = lock(&self.pending_recordings);
        let active = routes.values().flatten().any(|route| {
            route.session_id == session_id
                && route.consumer == consumer
                && route
                    .recorded
                    .as_ref()
                    .is_some_and(|item| Arc::ptr_eq(&item.journal, &recorded.journal))
        });
        let waiting = pending
            .get(&(session_id, consumer.to_owned()))
            .is_some_and(|item| Arc::ptr_eq(&item.journal, &recorded.journal));
        if (active || waiting)
            && let Some(session) = self.session(session_id)
        {
            session
                .outbox
                .recording_changed(consumer, &recorded.recording_id);
        }
    }

    pub(crate) fn remove_route(&self, scope: &InstanceKey, session_id: u64, consumer: &str) {
        let mut routes = lock(&self.routes);
        if let Some(entries) = routes.get_mut(scope) {
            entries.retain(|entry| !(entry.session_id == session_id && entry.consumer == consumer));
            if entries.is_empty() {
                routes.remove(scope);
            }
        }
        if let Some(session) = self.session(session_id) {
            session.outbox.remove_recording_hints(consumer);
        }
    }

    /// Queue a flush of `scope` (values admitted before a route existed).
    pub(crate) fn kick(&self, scope: InstanceKey) {
        self.signals.push_value(scope);
    }

    pub(crate) fn remove_session(&self, session_id: u64) {
        let mut ingress = lock(&self.scan_ingress);
        if let Some(session) = lock(&self.sessions).remove(&session_id)
            && (ingress.pending_by_session.contains_key(&session_id)
                || session.outbox.pending_scan_failure_owners() != 0
                || self.has_scan_terminal_evidence_debt(session_id))
        {
            ingress.retired_sessions.insert(session_id, session);
        }
        drop(ingress);
        let publications = {
            let mut publications = lock(&self.subscription_terminal_evidence_publications);
            let reservation_ids = publications
                .iter()
                .filter_map(|(reservation_id, publication)| {
                    (publication.route.session_id == session_id).then(|| reservation_id.clone())
                })
                .collect::<Vec<_>>();
            reservation_ids
                .into_iter()
                .filter_map(|reservation_id| publications.remove(&reservation_id))
                .collect::<Vec<_>>()
        };
        for publication in publications {
            self.abandon_subscription_terminal_evidence_or_retain(publication);
        }
    }

    pub(crate) fn note_background(&self, scope: &BackgroundScope, lease_id: String) {
        lock(&self.background)
            .entry(scope.clone())
            .or_default()
            .insert(lease_id);
    }

    pub(crate) fn holds_background(&self, scope: &BackgroundScope, lease_id: &str) -> bool {
        lock(&self.background)
            .get(scope)
            .is_some_and(|leases| leases.contains(lease_id))
    }

    /// Release one lease through the platform; forgotten only once the
    /// platform confirmed it.
    pub(crate) async fn release_background(
        &self,
        scope: &BackgroundScope,
        lease_id: &str,
        ctl: &OpControl,
    ) -> Result<(), DesktopError> {
        let lease = lease_id.to_owned();
        match crate::session::bounded(
            ctl,
            "background.release",
            self.radio
                .call(|id| crate::radio::RadioRequest::ReleaseBackground {
                    id,
                    lease_id: lease,
                }),
        )
        .await?
        {
            RadioCompletion::Unit => {
                let mut background = lock(&self.background);
                if let Some(leases) = background.get_mut(scope) {
                    leases.remove(lease_id);
                    if leases.is_empty() {
                        background.remove(scope);
                    }
                }
                Ok(())
            }
            _ => Err(crate::session::protocol("background.release")),
        }
    }

    /// Release every lease of `scope`; each failure is reported and the
    /// lease kept for a retry.
    pub(crate) async fn release_background_scope(&self, scope: &BackgroundScope) -> Vec<Value> {
        let leases: Vec<String> = lock(&self.background)
            .get(scope)
            .map(|leases| leases.iter().cloned().collect())
            .unwrap_or_default();
        let mut failures = Vec::new();
        for lease_id in leases {
            if let Err(error) = self
                .release_background(scope, &lease_id, &OpControl::unbounded())
                .await
            {
                failures.push(crate::session::cleanup_failure("background", &error));
            }
        }
        failures
    }
}

fn union_or_broad<'a>(sets: impl Iterator<Item = &'a Vec<String>>) -> Vec<String> {
    let mut union = Vec::new();
    for set in sets {
        if set.is_empty() {
            return Vec::new();
        }
        union.extend(set.iter().cloned());
    }
    union
}

fn covers(physical: &[String], wanted: &[String]) -> bool {
    physical.is_empty()
        || (!wanted.is_empty()
            && wanted
                .iter()
                .all(|item| physical.iter().any(|p| p.eq_ignore_ascii_case(item))))
}

fn sort_dedup(items: &mut Vec<String>) {
    items.sort();
    items.dedup();
}

static NEXT_CLOCK_EPOCH: AtomicU64 = AtomicU64::new(1);
static NEXT_EXACT_SESSION: AtomicU64 = AtomicU64::new(1);
static NEXT_TERMINAL_EVIDENCE_RECOVERY_LEASE: AtomicU64 = AtomicU64::new(1);

fn utc_ms(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64() * 1000.0,
        Err(error) => -(error.duration().as_secs_f64() * 1000.0),
    }
}

/// Capture at Rust host ingress, before queue admission, never at JavaScript drain.
fn native_receipt(inner: &HostInner) -> ubm_core::central::NotificationReceipt {
    ubm_core::central::NotificationReceipt {
        received_at_utc_ms: utc_ms(SystemTime::now()),
        received_at_monotonic_ms: inner.clock.elapsed().as_secs_f64() * 1000.0,
        clock_id: inner.clock_id.clone(),
    }
}

fn receipt_value(receipt: ubm_core::central::NotificationReceipt) -> Value {
    object(vec![
        ("receivedAtUtcMs", Value::from(receipt.received_at_utc_ms)),
        (
            "receivedAtMonotonicMs",
            Value::from(receipt.received_at_monotonic_ms),
        ),
        ("clockId", Value::from(receipt.clock_id)),
        ("origin", Value::from("native")),
    ])
}

async fn pump(host: Weak<HostInner>, signals: Arc<Signals>) {
    loop {
        while let Some(signal) = signals.pop() {
            let Some(host) = host.upgrade() else {
                return;
            };
            let continuation_changed = match &signal {
                HostSignal::Lifecycle(event) => {
                    !matches!(event.kind, LifecycleKind::Released { .. })
                }
                HostSignal::AdapterReset(..) | HostSignal::Adapter(..) => true,
                _ => false,
            };
            {
                // One awaited signal pass owns order. The existing bounded
                // signal/native queues retain pressure while disk is busy.
                // A journal may attach while handle awaits; never choose the
                // execution thread from a pre-await attachment snapshot.
                let owned = host.clone();
                let runtime = host.runtime.clone();
                if let Err(error) =
                    ubm_desktop::continuation_journal::run_blocking_result(move || {
                        runtime.block_on(owned.handle(signal));
                        Ok(())
                    })
                    .await
                {
                    host.fail_pump(error).await;
                    return;
                }
            }
            if continuation_changed
                && !host.shut_down.load(Ordering::SeqCst)
                && let Some(executor) = host.continuation.get()
            {
                executor.request_recovery(&host.runtime);
            }
        }
        // Owner failure must not terminate the pump while accepted scan
        // work is waiting for its held admission or publication.
        let deferred_scan = host.upgrade().is_some_and(|host| {
            let ingress = lock(&host.scan_ingress);
            !ingress.deferred_failures.is_empty()
                || ingress.deferred_reset.is_some()
                || !ingress.pending_by_session.is_empty()
        });
        if !deferred_scan
            && let Some((domain, operation, detail)) = signals.take_source_ingress_failure()
        {
            if let Some(host) = host.upgrade() {
                host.fail_pump(serde_json::json!({"code":"platform.failure", "domain":domain, "operation":operation, "detail":detail})).await;
            }
            return;
        }
        // Overflow is never silently discarded (X-R6): a lost lifecycle or
        // current-state fact becomes one control ingress-drop per session,
        // which drives `session.reconcile` from retained owner truth; lost
        // ingress drops keep their class so drop accounting stays exact.
        let (lost, drops) = signals.take_overflow();
        if lost > 0 || drops != [0, 0, 0] {
            let Some(host) = host.upgrade() else {
                return;
            };
            let sessions = host.session_list();
            let report_host = host.clone();
            let runtime = host.runtime.clone();
            let report = move || {
                for (index, class) in INGRESS_CLASSES.iter().enumerate() {
                    if drops[index] > 0 {
                        runtime.block_on(report_host.record_ingress_drop(*class, drops[index]));
                    }
                }
                if lost > 0 {
                    for session in &sessions {
                        session.outbox.push_ingress_drop(IngressClass::Control);
                    }
                }
                for (index, class) in INGRESS_CLASSES.iter().enumerate() {
                    let count = drops[index];
                    if count > 0 {
                        for session in &sessions {
                            session.outbox.push_ingress_drop_count(*class, count);
                        }
                    }
                }
            };
            {
                if let Err(error) =
                    ubm_desktop::continuation_journal::run_blocking_result(move || {
                        report();
                        Ok(())
                    })
                    .await
                {
                    host.fail_pump(error).await;
                    return;
                }
            }
        }
        if signals.is_closed() {
            return;
        }
        signals.notify.notified().await;
    }
}

fn ordered_consumer_for_ready(
    host: &HostInner,
    ready: &ordered_wake::OrderedReady,
) -> Option<Arc<ordered_batch::OrderedBatchConsumer>> {
    lock(&host.ordered_consumers)
        .iter()
        .find_map(|(source, consumer)| {
            (source.native_group == ready.group_id && source.generation == ready.generation)
                .then(|| Arc::clone(consumer))
        })
}

fn fail_ordered_consumer(consumer: &ordered_batch::OrderedBatchConsumer, error: DesktopError) {
    if consumer.failed() {
        return;
    }
    if let Err(report_error) = consumer.record_failure(&error) {
        consumer.retain_unreported_failure(error, report_error);
    }
    consumer.mark_failed();
}

async fn ordered_wake_pump(host: Weak<HostInner>, wakes: Arc<ordered_wake::OrderedWakeRegistry>) {
    let mut workers = tokio::task::JoinSet::new();
    let mut running = HashMap::new();
    loop {
        let Some(owner) = host.upgrade() else {
            return;
        };
        while let Some(ready) = wakes.take_mismatch_failure() {
            if let Some(consumer) = ordered_consumer_for_ready(&owner, &ready) {
                fail_ordered_consumer(
                    &consumer,
                    DesktopError::new(
                        BleErrorCode::ProtocolViolation,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.wake",
                    )
                    .with_detail(
                        "native wake generation does not match the bound source".to_owned(),
                    ),
                );
            } else if !wakes.is_retiring(ready.group_id) {
                owner.fail_pump(serde_json::json!({"code":"platform.failure", "domain":"gatt", "operation":"gatt.ordered.source-owner"})).await;
                return;
            }
        }
        while let Some(ready) = wakes.take() {
            let Some(consumer) = ordered_consumer_for_ready(&owner, &ready) else {
                if wakes.is_retiring(ready.group_id) {
                    let _ = wakes.finish(&ready);
                    continue;
                }
                owner.fail_pump(serde_json::json!({"code":"platform.failure", "domain":"gatt", "operation":"gatt.ordered.source-owner"})).await;
                return;
            };
            let worker_ready = ready.clone();
            let task = workers.spawn(async move {
                let result = if consumer.failed() {
                    Ok(())
                } else {
                    loop {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(1),
                            consumer.drain_to_empty(),
                        )
                        .await
                        {
                            Ok(Ok(true)) => break Ok(()),
                            Ok(Ok(false)) | Err(_) => {
                                // The native batch remains owned while a public
                                // poll frees capacity. Do not hold the delivery
                                // gate, and do not require another native wake.
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            }
                            Ok(Err(error)) => break Err(error),
                        }
                    }
                };
                (worker_ready, consumer, result)
            });
            running.insert(task.id(), ready);
        }
        if wakes.is_closed() {
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            return;
        }
        drop(owner);
        tokio::select! {
            _ = wakes.notified() => {},
            result = workers.join_next_with_id(), if !workers.is_empty() => {
                let Some(owner) = host.upgrade() else { return; };
                match result {
                    Some(Ok((id, (ready, consumer, result)))) => {
                        running.remove(&id);
                        if let Err(error) = result {
                            // Admission capacity is not a native source failure. Keep
                            // the retained batch and offset for a later exact retry;
                            // the final drain returns this error to the caller.
                            if !ordered_batch::is_admission_refusal(&error) {
                                fail_ordered_consumer(&consumer, error);
                            }
                        }
                        if wakes.finish(&ready).is_err() && !wakes.is_closed() {
                            fail_ordered_consumer(&consumer, DesktopError::new(BleErrorCode::LifecycleInvariantViolation, BleErrorDomain::Gatt, "gatt.ordered.wake"));
                        }
                    }
                    Some(Err(error)) => {
                        if let Some(ready) = running.remove(&error.id()) {
                            if let Some(consumer) = ordered_consumer_for_ready(&owner, &ready) {
                                fail_ordered_consumer(&consumer, DesktopError::new(BleErrorCode::PlatformFailure, BleErrorDomain::Gatt, "gatt.ordered.worker").with_detail("ordered source worker failed".to_owned()));
                            }
                            let _ = wakes.finish(&ready);
                        } else {
                            owner.fail_pump(serde_json::json!({"code":"platform.failure", "domain":"gatt", "operation":"gatt.ordered.worker"})).await;
                            return;
                        }
                    }
                    None => {},
                }
            }
        }
    }
}

// Only abandoned opens are fenced. A stalled source must not hold another
// group's retry turn, and native retirement must not discard central cleanup.
async fn ordered_recovery_pump(host: Weak<HostInner>, stop: Arc<Notify>) {
    use ubm_desktop::RadioBoundary;
    use ubm_desktop::central::OrderedCleanupOutcome;
    let mut delay = std::time::Duration::from_millis(500);
    let mut tasks = tokio::task::JoinSet::new();
    let mut running = HashMap::new();
    let mut active = HashSet::new();
    // Entries outlive native slot retirement until central cleanup settles.
    let mut cleanup_due = HashMap::<u64, String>::new();
    let mut stopping = false;
    loop {
        if stopping && tasks.is_empty() {
            return;
        }
        tokio::select! {
            () = stop.notified(), if !stopping => stopping = true,
            result = tasks.join_next_with_id(), if !tasks.is_empty() => {
                match result {
                    Some(Ok((id, (group, complete)))) => {
                        running.remove(&id);
                        active.remove(&group);
                        if complete {
                            cleanup_due.remove(&group);
                            if let Some(owner) = host.upgrade() {
                                owner.radio.complete_v2_central_cleanup(group);
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if let Some(group) = running.remove(&error.id()) {
                            active.remove(&group);
                            if let Some(owner) = host.upgrade() {
                                let failure = DesktopError::new(
                                    BleErrorCode::LifecycleInvariantViolation,
                                    BleErrorDomain::Cleanup,
                                    "gatt.ordered.recovery-worker",
                                ).with_detail(format!("ordered recovery task failed: {error}"));
                                if owner.radio.retained_ordered_open(group).is_none() {
                                    eprintln!("ubm-mobile: central ordered cleanup worker failed: {failure}");
                                }
                                owner.radio.record_v2_recovery_error(group, failure);
                            }
                        }
                    }
                    None => {}
                }
            }
            () = tokio::time::sleep(delay), if !stopping => {
                let Some(owner) = host.upgrade() else { return; };
                for (group, peer) in owner.radio.abandoned_v2_opens() {
                    cleanup_due.entry(group).or_insert(peer);
                }
                delay = if cleanup_due.is_empty() {
                    std::time::Duration::from_millis(500)
                } else {
                    (delay * 2).min(std::time::Duration::from_secs(30))
                };
                for (&group, peer) in &cleanup_due {
                    if !active.insert(group) { continue; }
                    let peer = peer.clone();
                    let owner = Arc::clone(&owner);
                    let task = tasks.spawn(async move {
                        let recovery = owner.radio.recover_ordered_source(&peer, group).await;
                        let complete = match recovery {
                            Ok(ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
                                | ubm_desktop::boundary::OrderedSourceRecovery::NotStarted) => {
                                // Some boundary-only opens have no central batch.
                                match owner.central.ordered_batch_cleanup_complete(&peer, group).await {
                                    Ok(true) => true,
                                    Err(error) if error.code() == BleErrorCode::OwnershipDenied => true,
                                    Err(error) => {
                                        eprintln!("ubm-mobile: central ordered cleanup status failed: {error}");
                                        owner.radio.record_v2_recovery_error(group, error);
                                        false
                                    }
                                    Ok(false) => match owner.central.retry_retained_ordered_batch(
                                        &peer, group, OpControl::budget_ms(1000),
                                    ).await {
                                        Ok(OrderedCleanupOutcome::Released) => true,
                                        Ok(_) => false,
                                        Err(error) => {
                                            if owner.radio.retained_ordered_open(group).is_none() {
                                                eprintln!("ubm-mobile: central ordered cleanup remains retained: {error}");
                                            }
                                            owner.radio.record_v2_recovery_error(group, error);
                                            false
                                        }
                                    },
                                }
                            }
                            Ok(_) => false,
                            Err(error) => {
                                owner.radio.record_v2_recovery_error(group, error);
                                false
                            }
                        };
                        (group, complete)
                    });
                    running.insert(task.id(), group);
                }
            }
        }
    }
}

impl HostInner {
    #[cfg(test)]
    pub(crate) fn stop_ordered_recovery_for_test(&self) {
        self.ordered_recovery_stop.notify_one();
    }

    pub(crate) fn ordered_consumers_for_peer(
        &self,
        peer_id: &str,
    ) -> Vec<(
        ubm_desktop::ordered_notification_groups::SourceKey,
        Arc<ordered_batch::OrderedBatchConsumer>,
    )> {
        lock(&self.ordered_consumers)
            .iter()
            .filter(|(source, _)| source.peer_id == peer_id)
            .map(|(source, consumer)| (source.clone(), Arc::clone(consumer)))
            .collect()
    }

    pub(crate) fn ordered_consumer_for_lease(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
    ) -> Option<Arc<ordered_batch::OrderedBatchConsumer>> {
        let source = ubm_desktop::ordered_notification_groups::SourceKey {
            peer_id: lease.peer_id.clone(),
            generation: lease.source_generation.clone(),
            native_group: lease.native_group,
        };
        lock(&self.ordered_consumers).get(&source).cloned()
    }

    /// Call only after native removal and all physical cleanup are confirmed.
    pub(crate) fn retire_ordered_source(&self, lease: &ubm_desktop::boundary::OrderedSourceLease) {
        let source = ubm_desktop::ordered_notification_groups::SourceKey {
            peer_id: lease.peer_id.clone(),
            generation: lease.source_generation.clone(),
            native_group: lease.native_group,
        };
        lock(&self.ordered_consumers).remove(&source);
        self.ordered_wakes.remove(lease.native_group);
    }

    /// Attach only after the central has installed this exact lease under its
    /// connection/database admission, before physical member enablement.
    pub(crate) fn attach_ordered_source(
        self: &Arc<Self>,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
    ) -> Result<(), DesktopError> {
        let matches_group = match self.ordered_group(lease.collection_id) {
            Some(mapping) => mapping.native_group_id == lease.native_group,
            None => lease.collection_id == lease.native_group,
        };
        if !matches_group {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolViolation,
                BleErrorDomain::Gatt,
                "gatt.ordered.attach",
            ));
        }
        let source = ubm_desktop::ordered_notification_groups::SourceKey {
            peer_id: lease.peer_id.clone(),
            generation: lease.source_generation.clone(),
            native_group: lease.native_group,
        };
        let mut consumers = lock(&self.ordered_consumers);
        if consumers.contains_key(&source) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.attach",
            ));
        }
        consumers.insert(
            source.clone(),
            Arc::new(ordered_batch::OrderedBatchConsumer::attach(
                self,
                source.clone(),
                lease.collection_id,
                lease.members.is_empty(),
            )),
        );
        if self
            .ordered_wakes
            .bind(source.native_group, source.generation.clone())
            .is_err()
        {
            consumers.remove(&source);
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.wake",
            ));
        }
        Ok(())
    }
}

impl MobileHost {
    /// Open terminal recovery for the process-selected native identity.  This
    /// is intentionally zero-argument: JavaScript cannot select another
    /// manager, recording or evidence reservation by guessing an identifier.
    pub fn terminal_evidence_recovery_selected(
        &self,
    ) -> Result<TerminalEvidenceRecovery, DesktopError> {
        if self.inner.is_shut_down() {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "terminal-evidence-recovery.open",
            ));
        }
        let ledger = self.inner.terminal_evidence_ledger().ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Core,
                "terminal-evidence-recovery.open",
            )
            .with_detail("trusted terminal evidence storage was not configured")
        })?;
        Ok(TerminalEvidenceRecovery {
            host: Arc::clone(&self.inner),
            lease: Arc::new(TerminalEvidenceRecoveryLease {
                id: format!(
                    "terminal-evidence-recovery-{}",
                    NEXT_TERMINAL_EVIDENCE_RECOVERY_LEASE.fetch_add(1, Ordering::Relaxed)
                ),
                ledger,
                admission: Mutex::new(()),
                closed: AtomicBool::new(false),
                #[cfg(test)]
                before_list_claim: Mutex::new(None),
                selected: Mutex::new(HashMap::new()),
                acknowledged: Mutex::new((VecDeque::new(), HashSet::new())),
            }),
        })
    }

    /// Attaches the process-selected durable terminal ledger to this live
    /// host. Replaying the process's same configured bound is idempotent;
    /// conflicting bounds are rejected by the process configuration authority.
    pub fn configure_terminal_evidence_maximum_bytes(
        &self,
        maximum_bytes: u64,
    ) -> Result<(), DesktopError> {
        crate::continuation::configure_terminal_evidence_maximum_bytes(maximum_bytes).map_err(
            |detail| {
                DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "mobile.terminal-evidence.configure",
                )
                .with_detail(detail)
            },
        )?;
        self.inner.attach_terminal_evidence(maximum_bytes)
    }

    /// Makes the next scan terminal durable commit fail. Available only to
    /// test builds and the explicit `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_scan_terminal_evidence_commit_for_test(&self) {
        self.inner
            .scan_terminal_evidence_commit_failures_remaining
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Number of native-owned scan terminals waiting for their durable commit.
    /// Available only to test builds and the explicit `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn pending_scan_terminal_evidence_debts_for_test(&self) -> usize {
        lock(&self.inner.scan_terminal_evidence_debts).len()
    }

    /// Makes the next subscription terminal durable commit fail. Available
    /// only to test builds and the explicit `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_subscription_terminal_evidence_commit_for_test(&self) {
        self.inner
            .subscription_terminal_evidence_commit_failures_remaining
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Number of native-owned subscription terminals waiting for durable
    /// commit. Available only to test builds and `test-support`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn pending_subscription_terminal_evidence_debts_for_test(&self) -> usize {
        lock(&self.inner.subscription_terminal_evidence_debts).len()
    }

    /// Seeds one durable terminal only for foreign-boundary recovery tests.
    ///
    /// This keeps the JVM harness on the actual host-selected SQLite ledger:
    /// it does not manufacture a recovery entry or bypass recipient fencing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_terminal_evidence_for_test(
        &self,
        producer: &str,
        manager_id: &str,
        operation_id: &str,
        incarnation: &str,
        canonical_bytes: &[u8],
        commit: bool,
    ) -> Result<(), DesktopError> {
        let ledger = self.inner.terminal_evidence_ledger().ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Core,
                "terminal-evidence-recovery.test-seed",
            )
            .with_detail("trusted terminal evidence storage was not configured")
        })?;
        let scope = TerminalEvidenceScope {
            producer: producer.to_owned(),
            manager_id: manager_id.to_owned(),
            operation_id: operation_id.to_owned(),
            incarnation: incarnation.to_owned(),
            scope_utf16le: None,
        };
        let reservation = ledger
            .reserve(
                &scope,
                u64::try_from(canonical_bytes.len()).map_err(|_| {
                    DesktopError::new(
                        BleErrorCode::BytesTooLarge,
                        BleErrorDomain::Stream,
                        "terminal-evidence-recovery.test-seed",
                    )
                })?,
            )
            .map_err(terminal_recovery_error)?;
        ledger
            .stage(
                &reservation.reservation_id,
                &self.inner.terminal_evidence_recipient,
                canonical_bytes,
            )
            .map_err(terminal_recovery_error)?;
        if commit {
            ledger
                .commit(
                    &reservation.reservation_id,
                    canonical_bytes,
                    Sha256::digest(canonical_bytes).into(),
                )
                .map_err(terminal_recovery_error)?;
        }
        // This test-only helper models evidence left behind after its source
        // has stopped. Keep the production live-source reservation semantics
        // intact: contention tests reserve directly and do not call this
        // helper.
        ledger
            .abandon(&reservation.reservation_id)
            .map_err(terminal_recovery_error)?;
        Ok(())
    }

    /// Enables only the explicit debug-only synthetic radio harness.
    #[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
    pub fn enable_synthetic_marker_for_test(&self) {
        self.inner.radio.enable_marker_for_test();
    }

    #[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
    pub fn synthetic_marker_enabled_for_test(&self) -> bool {
        self.inner.radio.synthetic_marker_enabled()
    }

    /// Retained process-level source failure, including its attempted cleanup.
    /// Reading this does not clear the diagnostic or acknowledge any recording.
    #[must_use]
    pub fn ingress_failure(&self) -> Option<Value> {
        lock(&self.inner.ingress_failure).clone()
    }
    /// Open the process owner: the central over `platform` on `runtime`
    /// (the shared desktop executor in production). Open issues no radio
    /// request, so the platform can answer requests only after it holds
    /// the returned host.
    pub async fn open(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
    ) -> Result<Self, DesktopError> {
        Self::open_with_recording_registry(platform, wake, options, runtime, Arc::default()).await
    }

    /// Install the radio using the same authority as pre-existing offline data access.
    pub async fn open_with_recording_registry(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
        recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> Result<Self, DesktopError> {
        if options.owner.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "ubm-mobile.host.owner",
            ));
        }
        let terminal_evidence = match crate::continuation::terminal_evidence_maximum_bytes() {
            None => None,
            Some(maximum) => Some(Arc::new(
                recording_registry
                    .terminal_evidence_ledger(maximum)
                    .map_err(|error| {
                        DesktopError::new(
                            BleErrorCode::CapabilityUnavailable,
                            BleErrorDomain::Core,
                            "ubm-mobile.host.terminal-evidence",
                        )
                        .with_detail(error.to_string())
                    })?,
            )),
        };
        let mut scan_finalization_secret = [0; 32];
        getrandom::fill(&mut scan_finalization_secret).map_err(|_| {
            DesktopError::new(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Core,
                "ubm-mobile.host.scan-finalization",
            )
            .with_detail("unable to allocate the process-owned scan finalization secret")
        })?;
        let radio = ForeignRadio::new(platform, options.platform, options.adapter_label.clone());
        let ordered_admission_owner = radio.ordered_admission_owner();
        let ordered_retirement_owner = radio.ordered_owner_retirement_owner();
        let coordinator = radio.ordered_open_coordinator().map_err(|error| {
            DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "ubm-mobile.host.ordered-owner",
            )
            .with_detail(format!("ordered admission owner unavailable: {error:?}"))
        })?;
        let signals = Arc::new(Signals::default());
        let observer_signals = Arc::clone(&signals);
        let observer_host: Arc<Mutex<Weak<HostInner>>> = Arc::new(Mutex::new(Weak::new()));
        let reset_host = Arc::clone(&observer_host);
        let observer: ubm_desktop::CentralObserver = Arc::new(move |signal| match signal {
            CentralSignal::Advertisement(_) => observer_signals.push(HostSignal::Advertisements),
            CentralSignal::Value { scope, .. }
            | CentralSignal::NotificationSourceFailed { scope }
            | CentralSignal::PeerSourceLoss { scope } => observer_signals.push_value(scope),
            CentralSignal::Lifecycle(event) => {
                observer_signals.push(HostSignal::Lifecycle(event));
            }
            // Adapter records come from the platform's full snapshot
            // (authorization, resetting), not the central's projection.
            CentralSignal::Adapter(_) => {}
            // The mobile radio opts into the core's adapter-loss teardown
            // (legacy `startAdapterLossCleanup`/`advanceGeneration`).
            CentralSignal::AdapterReset(event) => {
                let host = lock(&reset_host).upgrade();
                let captured = host.as_ref().is_some_and(|host| {
                    host.capture_reset_scan(&event.previous, event.ended_scan.as_ref(), event.cause)
                });
                let resources = host
                    .map(|host| {
                        host.session_list()
                            .into_iter()
                            .map(|session| {
                                let resources = session.capture_reset_resources(
                                    &event.previous,
                                    &event.released_links,
                                    &event.link_release_failures,
                                );
                                (session, resources)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                observer_signals.push(HostSignal::AdapterReset(
                    event.current,
                    if captured { None } else { event.ended_scan },
                    event.cause,
                    resources,
                ));
            }
            // Security, scan-terminal, and connection-parameter facts are
            // not a mobile readiness stream. Apple write readiness is:
            // the radio probes `canSendWriteWithoutResponse` and ingests
            // `peripheralIsReady(toSendWriteWithoutResponse:)`.
            CentralSignal::WriteReadiness(event) => {
                observer_signals.push(HostSignal::WriteReadiness(
                    event.peer_id,
                    event.connection_generation,
                    event.ready,
                ));
            }
            CentralSignal::Security(_)
            | CentralSignal::ScanTerminal(_)
            | CentralSignal::ConnectionParameters(_)
            // This host currently has no public ownerless-diagnostic surface.
            // It must not turn an uncorrelated source observation into a
            // connection lifecycle event.
            | CentralSignal::SourceUncertainty(_) => {}
        });
        let drop_signals = Arc::clone(&signals);
        radio.set_drop_hook(Arc::new(move |class, count| {
            drop_signals.push(HostSignal::IngressDrop(class, count));
        }));
        let profile = CentralProfile {
            directory_os: ubm_desktop::DesktopOs::MacOs,
            identity: Arc::new(MobileIdentity::new(options.platform)),
            register_capabilities: register_mobile_capabilities,
            observer: Some(observer),
            adapter_id: None,
            bluez_bus: ubm_desktop::BluezBus::System,
            // Mobile hosts do not implicitly bind a desktop continuation
            // journal.  A host that needs retained peer-failure recovery must
            // opt in with its own explicit durable ledger.
            peer_failure_recovery_ledger: None,
        };
        let central = match DesktopCentral::open_with(radio.clone(), profile).await {
            Ok(central) => central,
            Err(error) => {
                if let Err(cleanup) = radio.confirm_unpublished_ordered_radio() {
                    return Err(DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Cleanup,
                        "ubm-mobile.host.ordered-owner",
                    )
                    .with_detail(format!(
                        "host open failed: {error:?}; unpublished radio cleanup failed: {cleanup:?}"
                    )));
                }
                return Err(error);
            }
        };
        let ordered_group_owner = match coordinator
            .map(|owner| owner.register_host().map(|host| (owner, host)))
            .transpose()
        {
            Ok(owner) => owner,
            Err(error) => {
                if let Err(cleanup) = radio.confirm_unpublished_ordered_radio() {
                    return Err(DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Cleanup,
                        "ubm-mobile.host.ordered-owner",
                    ).with_detail(format!("host registration failed: {error:?}; unpublished radio cleanup failed: {cleanup:?}")));
                }
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "ubm-mobile.host.ordered-owner",
                )
                .with_detail(format!("host registration failed: {error:?}")));
            }
        };
        let inner = Arc::new(HostInner {
            platform: options.platform,
            ordered_admission_owner,
            ordered_retirement_owner,
            ordered_group_owner,
            runtime: runtime.clone(),
            radio,
            central,
            wake: Arc::clone(&wake),
            signals: Arc::clone(&signals),
            ordered_wakes: Arc::new(ordered_wake::OrderedWakeRegistry::default()),
            ordered_owners: crate::ordered_owners::OrderedOwners::default(),
            ordered_start_receipts: Mutex::new(HashMap::new()),
            ordered_recording_start_guard: tokio::sync::Mutex::new(()),
            ordered_recordings: Mutex::new(HashMap::new()),
            ordered_consumers: Mutex::new(HashMap::new()),
            ordered_pump: Mutex::new(None),
            ordered_recovery_worker: Mutex::new(None),
            ordered_recovery_stop: Arc::new(Notify::new()),
            sessions: Mutex::new(BTreeMap::new()),
            recovered_scan_recordings: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
            #[cfg(test)]
            before_session_admission: Mutex::new(None),
            #[cfg(test)]
            after_scan_start: Mutex::new(None),
            #[cfg(debug_assertions)]
            route_turns: Mutex::new(Vec::new()),
            #[cfg(debug_assertions)]
            scope_turns: Mutex::new(Vec::new()),
            scan: tokio::sync::Mutex::new(ScanShare::default()),
            scan_members: Mutex::new(BTreeMap::new()),
            scan_terminal_evidence_debts: Mutex::new(Vec::new()),
            subscription_terminal_evidence_debts: Mutex::new(Vec::new()),
            subscription_terminal_evidence_publications: Mutex::new(HashMap::new()),
            terminal_evidence_recipient: options.owner.clone(),
            #[cfg(any(test, feature = "test-support"))]
            scan_terminal_evidence_commit_failures_remaining: AtomicUsize::new(0),
            #[cfg(any(test, feature = "test-support"))]
            subscription_terminal_evidence_commit_failures_remaining: AtomicUsize::new(0),
            #[cfg(test)]
            subscription_terminal_evidence_after_commit: Mutex::new(None),
            scan_finalization_secret,
            scan_ingress: Mutex::new(ScanIngress::default()),
            routes: Mutex::new(HashMap::new()),
            pending_recordings: Mutex::new(HashMap::new()),
            recorded_drops_settled: AtomicU64::new(0),
            recorded_drops_notify: Notify::new(),
            directory: Mutex::new(BTreeMap::new()),
            restored: Mutex::new(BTreeMap::new()),
            restoration_claims: Mutex::new(BTreeMap::new()),
            security: Mutex::new(HashMap::new()),
            security_failures: Mutex::new(BTreeMap::new()),
            security_failure_revision: AtomicU64::new(0),
            adapter: Mutex::new(None),
            background: Mutex::new(BTreeMap::new()),
            link_ends: Mutex::new(BTreeMap::new()),
            database_changes: Mutex::new(BTreeMap::new()),
            shut_down: AtomicBool::new(false),
            session_admission_closed: AtomicBool::new(false),
            shutdown_state: tokio::sync::Mutex::new(HostShutdownState::default()),
            shutdown_owner_phase: tokio::sync::watch::channel(false).0,
            shutdown_observed_failures: Mutex::new(Vec::new()),
            shutdown_running: Mutex::new(false),
            shutdown_updates: tokio::sync::watch::channel(None).0,
            shutdown_completion: Mutex::new(ShutdownCompletion::default()),
            ingress_failure: Mutex::new(None),
            pump: Mutex::new(None),
            clock: Instant::now(),
            clock_id: format!(
                "ubm-mobile-native-host-{}-{}-{}",
                std::process::id(),
                utc_ms(SystemTime::now()),
                NEXT_CLOCK_EPOCH.fetch_add(1, Ordering::Relaxed)
            ),
            continuation: std::sync::OnceLock::new(),
            scan_failure_recovery_registry: Arc::new(
                ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry::new(
                    Arc::clone(&recording_registry),
                    Arc::clone(&wake),
                ),
            ),
            recording_registry,
            terminal_evidence: Mutex::new(terminal_evidence),
            continuation_closed: AtomicBool::new(false),
            continuation_admission: Mutex::new(()),
        });
        *lock(&observer_host) = Arc::downgrade(&inner);
        if let Err(error) = inner.radio.retain_ordered_host(Arc::clone(&inner)) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "ubm-mobile.host.ordered-owner",
            )
            .with_detail(format!(
                "ordered admission owner unavailable: {error:?}; radio cleanup remains retained"
            )));
        }
        inner
            .radio
            .set_connect_generation_hook(Arc::downgrade(&inner));
        let ordered_worker = runtime.spawn(ordered_wake_pump(
            Arc::downgrade(&inner),
            Arc::clone(&inner.ordered_wakes),
        ));
        *lock(&inner.ordered_pump) = Some(ordered_worker);
        if inner.radio.uses_v2_admission() {
            *lock(&inner.ordered_recovery_worker) = Some(runtime.spawn(ordered_recovery_pump(
                Arc::downgrade(&inner),
                Arc::clone(&inner.ordered_recovery_stop),
            )));
        }
        let worker = runtime.spawn(pump(Arc::downgrade(&inner), signals));
        *lock(&inner.pump) = Some(worker);
        Ok(Self { inner })
    }

    /// Open from a non-runtime thread (JNI/UniFFI entry): runs [`Self::open`]
    /// on `runtime` and waits for it.
    pub fn open_blocking(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
    ) -> Result<Self, DesktopError> {
        Self::open_blocking_with_recording_registry(
            platform,
            wake,
            options,
            runtime,
            Arc::default(),
        )
    }

    pub fn open_blocking_with_recording_registry(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
        recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> Result<Self, DesktopError> {
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(
            Self::open_with_recording_registry(
                platform,
                wake,
                options,
                runtime.clone(),
                recording_registry,
            )
            .then_send(tx),
        );
        rx.recv().unwrap_or_else(|_| {
            Err(DesktopError::new(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Core,
                "ubm-mobile.host.open",
            )
            .with_detail("open task ended without an answer"))
        })
    }

    #[must_use]
    pub fn platform(&self) -> MobilePlatform {
        self.inner.platform
    }

    /// Return the process-owned, host-neutral durable scan-recovery registry.
    /// Native hosts apply their trusted recording-ID policy before calling
    /// `attach_trusted`; this accessor never converts a JavaScript/renderer
    /// string into authority on its own.
    #[must_use]
    pub fn scan_failure_recovery_registry(
        &self,
    ) -> Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry> {
        Arc::clone(&self.inner.scan_failure_recovery_registry)
    }

    /// Admission owner negotiated when this host was opened.
    #[must_use]
    pub fn ordered_admission_owner(&self) -> Option<crate::radio::OrderedAdmissionOwner> {
        self.inner.ordered_admission_owner
    }

    pub(crate) fn ordered_open_coordinator(
        &self,
    ) -> Result<Option<Arc<crate::foreign::MobileOpenCoordinator>>, crate::ordered_open_slots::Error>
    {
        self.inner.radio.ordered_open_coordinator()
    }

    pub(crate) fn begin_ordered_owner_retirement(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        self.inner.radio.begin_ordered_owner_retirement()
    }

    /// Requires positive settlement for this host, including that its workers
    /// and callbacks cannot add further cleanup obligations.
    pub(crate) fn confirm_ordered_host_cleanup(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        self.inner
            .radio
            .confirm_ordered_host_cleanup(Arc::clone(&self.inner))
    }

    /// Only call after a positive native quiescence response for this owner.
    pub(crate) fn retire_ordered_owner_after_native_quiescence(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        self.inner
            .radio
            .retire_ordered_owner_after_native_quiescence()
    }

    /// Deliver the platform's answer to one request.
    pub fn complete(
        &self,
        request_id: RequestId,
        completion: RadioCompletion,
    ) -> crate::foreign::CompletionStatus {
        self.inner.radio.complete(request_id, completion)
    }

    /// Install an explicitly opened source and its unique batch consumer.
    pub async fn install_ordered_consumer(
        &self,
        source: ubm_desktop::ordered_notification_groups::SourceKey,
        owner: ubm_core::ordered_notifications::OrderedCollection,
        admission: &ubm_desktop::GattAdmission,
    ) -> Result<Arc<ordered_batch::OrderedBatchConsumer>, DesktopError> {
        let consumer = Arc::new(
            ordered_batch::OrderedBatchConsumer::install(
                Arc::clone(&self.inner),
                source.clone(),
                owner,
                admission,
            )
            .await?,
        );
        let mut consumers = lock(&self.inner.ordered_consumers);
        consumers.insert(source, Arc::clone(&consumer));
        Ok(consumer)
    }

    /// Recover the installed consumer, including its unacknowledged offset.
    pub fn ordered_consumer(
        &self,
        source: &ubm_desktop::ordered_notification_groups::SourceKey,
    ) -> Option<Arc<ordered_batch::OrderedBatchConsumer>> {
        lock(&self.inner.ordered_consumers).get(source).cloned()
    }

    /// Reserved readiness registry for the ordered source owner.
    pub fn ordered_wakes(&self) -> &ordered_wake::OrderedWakeRegistry {
        &self.inner.ordered_wakes
    }

    /// Admit an exact notification-source terminal into the control FIFO.
    ///
    /// Unlike ordinary control ingress, an occupied queue does not turn this
    /// terminal into a counted drop or shut the host down.  The native caller
    /// retains its immutable receipt on `Full`, `Closed`, or `Invalid` and may
    /// retry the same receipt; `Accepted` is the single ownership transfer.
    pub fn ingest_notification_source_failure(
        &self,
        instance: &Instance,
        epoch: u64,
        failure: &crate::PlatformFailure,
    ) -> crate::SourceFailureIngressStatus {
        use crate::SourceFailureIngressStatus as Status;
        if self.inner.is_shut_down() {
            return Status::Closed;
        }
        let (Ok(service_uuid), Ok(characteristic_uuid)) = (
            canonical_uuid(&instance.service_uuid),
            canonical_uuid(&instance.characteristic_uuid),
        ) else {
            return Status::Invalid;
        };
        match self.inner.radio.try_push_source_failure(
            ubm_desktop::RadioEvent::NotificationSourceFailed {
                peer_id: instance.peer_id.clone(),
                service_uuid,
                service_occurrence: instance.service_occurrence,
                characteristic_uuid,
                characteristic_occurrence: instance.characteristic_occurrence,
                epoch,
                error: failure.to_acknowledged_source_error(self.inner.platform),
            },
        ) {
            crate::foreign::SourceFailureQueueStatus::Accepted => Status::Accepted,
            crate::foreign::SourceFailureQueueStatus::Full => Status::Full,
            crate::foreign::SourceFailureQueueStatus::Closed => Status::Closed,
        }
    }

    /// Transfer an owned scan failure to ingress. A refusal returns the same
    /// error to the caller, including all native metadata and cleanup causes.
    pub fn ingest_scan_failure_owned(
        &self,
        error: DesktopError,
        scan_request_id: Option<RequestId>,
    ) -> Result<(), DesktopError> {
        self.inner
            .capture_scan_failure(error, scan_request_id)
            .map_err(|(_, error)| error)
    }

    /// Deliver one unsolicited platform fact. Never blocks.
    pub fn ingest(&self, ingress: RadioIngress) -> IngressStatus {
        let inner = &*self.inner;
        // A scan failure is an owned observation: refusal returns the same
        // typed error even when the host has already closed.
        let ingress = match ingress {
            RadioIngress::ScanFailed { detail } => {
                let error = DesktopError::new(
                    BleErrorCode::ScanStartFailed,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail(detail);
                return match inner.capture_scan_failure(error, None) {
                    Ok(()) => IngressStatus::Accepted,
                    Err((_, error)) => IngressStatus::ScanFailureRefused {
                        error,
                        closed: inner.is_shut_down(),
                    },
                };
            }
            RadioIngress::ScanFailedScoped {
                scan_request_id,
                detail,
            } => {
                let error = DesktopError::new(
                    BleErrorCode::ScanStartFailed,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail(detail);
                return match inner.capture_scan_failure(error, Some(scan_request_id)) {
                    Ok(()) => IngressStatus::Accepted,
                    Err((_, error)) => IngressStatus::ScanFailureRefused {
                        error,
                        closed: inner.is_shut_down(),
                    },
                };
            }
            RadioIngress::ScanFailedAndroid {
                scan_request_id,
                detail,
                native_code,
            } => {
                let platform =
                    ubm_desktop::errors::PlatformDetail::new("android", native_code.to_string())
                        .with_metadata(
                            "androidScanErrorCode",
                            ubm_desktop::errors::PlatformValue::Int(i64::from(native_code)),
                        );
                let error = DesktopError::new(
                    BleErrorCode::ScanStartFailed,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail(detail)
                .with_platform(platform);
                return match inner.capture_scan_failure(error, Some(scan_request_id)) {
                    Ok(()) => IngressStatus::Accepted,
                    Err((_, error)) => IngressStatus::ScanFailureRefused {
                        error,
                        closed: inner.is_shut_down(),
                    },
                };
            }
            other => other,
        };
        if inner.is_shut_down()
            && (inner.ordered_wakes.is_closed()
                || !matches!(
                    &ingress,
                    RadioIngress::OrderedWake { .. } | RadioIngress::OrderedWakeFailure { .. }
                ))
        {
            return IngressStatus::Closed;
        }
        let pushed = match ingress {
            RadioIngress::OrderedWake {
                generation,
                group_id,
            } => inner
                .ordered_wakes
                .wake(group_id, generation)
                .map_err(|_| IngressClass::Control),
            RadioIngress::OrderedWakeFailure {
                generation,
                group_id,
            } => {
                match inner.ordered_wakes.wake_failed(group_id, generation) {
                    Ok(()) => Ok(()),
                    Err(ordered_wake::OrderedWakeError::GenerationMismatch) => {
                        Err(IngressClass::Control)
                    }
                    Err(_) => {
                        // An unowned source cannot be attributed to a group.
                        inner.signals.fail_source_ingress(
                            "gatt",
                            "gatt.notification.source-ingress",
                            None,
                        );
                        Err(IngressClass::Control)
                    }
                }
            }
            RadioIngress::Advertisement(advertisement) => {
                inner.capture_advertisement(advertisement, None)
            }
            RadioIngress::AdvertisementScoped {
                scan_request_id,
                advertisement,
            } => inner.capture_advertisement(advertisement, Some(scan_request_id)),
            RadioIngress::Connection {
                peer_id,
                connected,
                status,
            } => {
                if !connected {
                    inner.radio.invalidate_gatt_snapshot(&peer_id);
                }
                inner.radio.push_event(if connected {
                    RadioEvent::Connected(peer_id)
                } else if status.is_some_and(|status| status != 0) {
                    // Android reports a non-zero GATT status, CoreBluetooth an
                    // `NSError`, when the link ended for a reason other than
                    // this app's release: a loss even if a release was pending.
                    RadioEvent::Lost(peer_id)
                } else {
                    RadioEvent::Disconnected(peer_id)
                })
            }
            RadioIngress::ConnectionScoped {
                peer_id,
                expected_generation,
                connected,
                status,
            } => {
                if expected_generation.is_empty() {
                    inner.radio.note_drop(IngressClass::Control);
                    Err(IngressClass::Control)
                } else {
                    inner.radio.push_event(RadioEvent::ConnectionScoped {
                        peer_id,
                        expected_generation,
                        connected,
                        errored: status.is_some_and(|status| status != 0),
                    })
                }
            }
            RadioIngress::ServicesChanged { peer_id } => {
                inner.radio.invalidate_gatt_snapshot(&peer_id);
                inner.radio.push_event(RadioEvent::ServicesChanged(peer_id))
            }
            RadioIngress::Notification {
                instance,
                epoch,
                value,
            } => match (
                canonical_uuid(&instance.service_uuid),
                canonical_uuid(&instance.characteristic_uuid),
            ) {
                (Ok(service_uuid), Ok(characteristic_uuid)) => {
                    let receipt = native_receipt(inner);
                    inner.radio.push_event(RadioEvent::NotificationWithReceipt {
                        peer_id: instance.peer_id,
                        service_uuid,
                        service_occurrence: instance.service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence: instance.characteristic_occurrence,
                        epoch,
                        value,
                        receipt,
                    })
                }
                _ => {
                    inner.radio.note_drop(IngressClass::Notification);
                    Err(IngressClass::Notification)
                }
            },
            RadioIngress::NotificationSourceFailed {
                instance,
                epoch,
                failure,
            } => match (
                canonical_uuid(&instance.service_uuid),
                canonical_uuid(&instance.characteristic_uuid),
            ) {
                (Ok(service_uuid), Ok(characteristic_uuid)) => {
                    let result = inner
                        .radio
                        .push_event(RadioEvent::NotificationSourceFailed {
                            peer_id: instance.peer_id,
                            service_uuid,
                            service_occurrence: instance.service_occurrence,
                            characteristic_uuid,
                            characteristic_occurrence: instance.characteristic_occurrence,
                            epoch,
                            error: failure.to_notification_source_error(inner.platform),
                        });
                    if result.is_err() {
                        inner.shut_down.store(true, Ordering::SeqCst);
                        inner.ordered_wakes.close();
                        inner.signals.fail_source_ingress(
                            "gatt",
                            "gatt.notification.source-ingress",
                            None,
                        );
                    }
                    result
                }
                _ => {
                    inner.radio.note_drop(IngressClass::Control);
                    inner.shut_down.store(true, Ordering::SeqCst);
                    inner.ordered_wakes.close();
                    inner.signals.fail_source_ingress(
                        "gatt",
                        "gatt.notification.source-ingress",
                        None,
                    );
                    Err(IngressClass::Control)
                }
            },
            RadioIngress::AdapterState(snapshot) => {
                if matches!(snapshot.power, AdapterPower::Off | AdapterPower::Resetting) {
                    inner.radio.invalidate_all_gatt_snapshots();
                }
                let updated_at = inner.now_ms();
                *lock(&inner.adapter) = Some((snapshot.clone(), updated_at));
                let state = central_adapter_state(&snapshot);
                // The change is published under the attachment it happened
                // in, queued before the central can reset it (legacy emitted
                // the state, then advanced the generation after cleanup).
                inner.signals.push(HostSignal::Adapter(
                    snapshot,
                    updated_at,
                    inner.central.attachment(),
                ));
                inner.radio.push_event(RadioEvent::AdapterState(state))
            }
            RadioIngress::ScanFailed { .. }
            | RadioIngress::ScanFailedScoped { .. }
            | RadioIngress::ScanFailedAndroid { .. } => {
                unreachable!("scan failures return before ordinary ingress")
            }
            RadioIngress::SecurityChanged { peer_id, state } => {
                inner.signals.push(HostSignal::Security(peer_id, state));
                Ok(())
            }
            RadioIngress::SecurityFailed { peer_id, failure } => {
                let error =
                    failure.to_error(crate::radio::RequestKind::SecurityState, inner.platform);
                let revision = inner
                    .security_failure_revision
                    .fetch_add(1, Ordering::SeqCst);
                lock(&inner.security_failures).insert(peer_id.clone(), (revision, error.clone()));
                inner
                    .signals
                    .push(HostSignal::SecurityFailed(peer_id, error));
                Ok(())
            }
            RadioIngress::WriteReadiness { peer_id, ready } => inner
                .radio
                .push_event(RadioEvent::WriteReadiness { peer_id, ready }),
            RadioIngress::Restored { peers } => {
                {
                    let mut restored = lock(&inner.restored);
                    for peer in &peers {
                        restored.insert(peer.peer_id.clone(), peer.clone());
                    }
                }
                inner.signals.push(HostSignal::Restored(peers));
                Ok(())
            }
            RadioIngress::Dropped { class, .. } => {
                inner.radio.note_drop(class);
                Err(class)
            }
            RadioIngress::DroppedCount { class, count, .. } => {
                inner.radio.note_drops(class, count);
                Err(class)
            }
        };
        match pushed {
            Ok(()) => IngressStatus::Accepted,
            Err(class) => IngressStatus::Dropped(class),
        }
    }

    /// Whether the debug-only observation sees an ingress fence held.
    #[cfg(debug_assertions)]
    pub fn recorded_ingress_paused(&self) -> bool {
        self.inner.radio.ingress_paused()
    }

    /// Consumers polled by the pump, in order. Debug tests use it.
    #[cfg(debug_assertions)]
    pub fn route_turns(&self) -> Vec<String> {
        lock(&self.inner.route_turns).clone()
    }

    /// The consumers whose group each value scope turn committed, in order.
    /// Debug tests use it.
    #[cfg(debug_assertions)]
    pub fn scope_turns(&self) -> Vec<Vec<String>> {
        lock(&self.inner.scope_turns).clone()
    }

    /// Open one session lease (one RN manager) that is its own background
    /// scope: `session.dispose` releases its background leases.
    pub fn open_session(&self, owner: &str) -> Result<MobileSession, DesktopError> {
        self.open_session_in(owner, None, Arc::clone(&self.inner.wake), None)
    }

    /// Native intake is consumed through an explicit claim, not a JavaScript
    /// wake route. Sharing the public wake sink would create orphan early wakes.
    pub(crate) fn open_native_session(&self, owner: &str) -> Result<MobileSession, DesktopError> {
        struct ClaimWake;
        impl WakeSink for ClaimWake {
            fn wake(&self, _: u64) {}
        }
        self.open_session_in(owner, None, Arc::new(ClaimWake), None)
    }

    /// Open one session lease whose background leases belong to the shared
    /// `scope` (one React Native module instance, 87/N8): they outlive the
    /// session and end with [`Self::release_background_scope`] or host
    /// shutdown.
    pub fn open_scoped_session(
        &self,
        owner: &str,
        scope: &str,
    ) -> Result<MobileSession, DesktopError> {
        if scope.is_empty() {
            return Err(wire::invalid("session.scope"));
        }
        self.open_session_in(owner, Some(scope), Arc::clone(&self.inner.wake), None)
    }

    fn open_session_in(
        &self,
        owner: &str,
        scope: Option<&str>,
        wake: Arc<dyn WakeSink>,
        exact_namespace: Option<String>,
    ) -> Result<MobileSession, DesktopError> {
        if owner.is_empty() {
            return Err(wire::invalid("session.owner"));
        }
        if self.inner.is_shut_down() || self.inner.session_admission_closed.load(Ordering::SeqCst) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.inner.before_session_admission).clone() {
            hook();
        }
        // The same guard fences the fatal source transition and publication.
        // An open already parsing options cannot publish after its snapshot.
        let mut sessions = lock(&self.inner.sessions);
        if self.inner.is_shut_down() || self.inner.session_admission_closed.load(Ordering::SeqCst) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            ));
        }
        let id = self.inner.allocate_session_id(&sessions)?;
        let background_scope = scope.map_or(BackgroundScope::Session(id), |scope| {
            BackgroundScope::Shared(scope.to_owned())
        });
        let state = Arc::new(SessionState::new(
            id,
            Outbox::new(id, wake),
            background_scope,
            exact_namespace,
        ));
        let mut recovery_authority = [0; 32];
        getrandom::fill(&mut recovery_authority).map_err(|_| scan_recovery_error())?;
        state
            .outbox
            .configure_scan_failure_recovery_authority(recovery_authority)
            .map_err(|_| scan_recovery_error())?;
        sessions.insert(id, Arc::clone(&state));
        Ok(MobileSession::new(Arc::clone(&self.inner), state))
    }

    /// Open a session after checking the caller speaks this wire revision
    /// (`protocol.incompatible` before any session exists otherwise), and
    /// answer the admission record `{sessionId, contractRevision,
    /// wireRevision, buildIdentity}` in one step. `build_identity_json` is
    /// the binding's `ubm-native-build-identity/1` record.
    pub fn admit_session(
        &self,
        owner: &str,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        self.admit(owner, None, expected_wire_revision, build_identity_json)
    }

    /// [`Self::admit_session`] for a session of the shared background
    /// `scope` (see [`Self::open_scoped_session`]).
    pub fn admit_scoped_session(
        &self,
        owner: &str,
        scope: &str,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        if scope.is_empty() {
            return Err(wire::invalid("session.scope"));
        }
        self.admit(
            owner,
            Some(scope),
            expected_wire_revision,
            build_identity_json,
        )
    }

    fn admit(
        &self,
        owner: &str,
        scope: Option<&str>,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        if expected_wire_revision != wire::WIRE_REVISION
            && expected_wire_revision != wire::EXACT_CONNECTION_WIRE_REVISION
        {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolIncompatible,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            )
            .with_detail(format!(
                "caller speaks {expected_wire_revision}, owner speaks {}",
                wire::WIRE_REVISION
            )));
        }
        let identity: Value = serde_json::from_str(build_identity_json).map_err(|_| {
            DesktopError::new(
                BleErrorCode::ProtocolMalformed,
                BleErrorDomain::Core,
                "ubm-mobile.session.build-identity",
            )
        })?;
        let exact_namespace = if expected_wire_revision == wire::EXACT_CONNECTION_WIRE_REVISION {
            let sequence = NEXT_EXACT_SESSION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| {
                    DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Core,
                        "ubm-mobile.session.identity",
                    )
                    .with_detail("session identity namespace exhausted")
                })?;
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Core,
                    "ubm-mobile.session.identity",
                )
                .with_detail("unable to allocate a session identity namespace")
            })?;
            let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            Some(format!("{nonce}:{sequence}"))
        } else {
            None
        };
        let session =
            self.open_session_in(owner, scope, Arc::clone(&self.inner.wake), exact_namespace)?;
        let record = object(vec![
            ("sessionId", Value::from(session.id())),
            (
                "contractRevision",
                Value::from(ubm_core::contracts::CONTRACT_REVISION),
            ),
            ("wireRevision", Value::from(expected_wire_revision)),
            ("buildIdentity", identity),
        ]);
        Ok((session, record.to_string()))
    }

    /// Look up a live session by id (binding handle tables).
    #[must_use]
    pub fn session(&self, id: u64) -> Option<MobileSession> {
        self.inner
            .session(id)
            .map(|state| MobileSession::new(Arc::clone(&self.inner), state))
    }

    /// Recover an active or disposed session using the authority acquired by
    /// that session before disposal. Session IDs alone are not authority.
    pub(crate) fn scan_failure_recovery(
        &self,
        id: u64,
        authority: &[u8; 32],
    ) -> Result<ScanFailureRecovery, DesktopError> {
        let state = self
            .inner
            .scan_session(id)
            .ok_or_else(scan_recovery_error)?;
        self.inner.authenticate_scan_authority(&state, authority)?;
        Ok(ScanFailureRecovery {
            host: Arc::clone(&self.inner),
            state,
        })
    }

    /// Reopen retained v2 scan-failure owners from one already configured,
    /// private recording. The recording id is resolved only through this
    /// host's injected registry; paths and raw sidecar secrets never cross the
    /// caller boundary. Legacy/mixed sidecars without a persisted capability
    /// remain export-only and fail closed.
    pub fn attach_scan_failure_recovery_recording(
        &self,
        recording_id: &str,
    ) -> Result<(), DesktopError> {
        if self.inner.is_shut_down() || self.inner.session_admission_closed.load(Ordering::SeqCst) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "scan.failure-recovery.attach",
            ));
        }
        let journal = self
            .inner
            .recording_registry
            .get(recording_id)
            .map_err(|_| scan_recovery_error())?;
        let identity = Outbox::recovery_authority_from_journal(&journal)
            .map_err(|_| scan_recovery_unsupported())?
            .ok_or_else(scan_recovery_unsupported)?;
        let mut sessions = lock(&self.inner.sessions);
        if let Some(local_id) = lock(&self.inner.recovered_scan_recordings)
            .get(recording_id)
            .copied()
        {
            let existing = sessions.get(&local_id).ok_or_else(scan_recovery_error)?;
            let current = existing
                .outbox
                .scan_failure_recovery_authority()
                .map_err(|_| scan_recovery_error())?;
            if current == Some(identity.authority) {
                return Ok(());
            }
            return Err(scan_recovery_error());
        }
        let local_id = self.inner.allocate_session_id(&sessions)?;
        let outbox = Outbox::new(identity.session_id, Arc::clone(&self.inner.wake));
        outbox
            .configure_scan_failure_recovery_authority(identity.authority)
            .map_err(|_| scan_recovery_error())?;
        outbox
            .attach_journal(
                journal,
                serde_json::json!({"kind":"scan-failure-recovery","recordingId":recording_id}),
            )
            .map_err(|_| scan_recovery_error())?;
        let restored = outbox
            .scan_failure_recovery_authority()
            .map_err(|_| scan_recovery_error())?;
        if restored != Some(identity.authority) {
            return Err(scan_recovery_error());
        }
        let state = Arc::new(SessionState::new(
            local_id,
            outbox,
            BackgroundScope::Session(local_id),
            None,
        ));
        sessions.insert(local_id, state);
        lock(&self.inner.recovered_scan_recordings).insert(recording_id.to_owned(), local_id);
        Ok(())
    }

    /// Trusted host-only recovery path. The caller supplies only an opaque
    /// recording id; the durable capability never crosses this boundary.
    pub fn scan_failure_recovery_for_recording(
        &self,
        recording_id: &str,
    ) -> Result<ScanFailureRecovery, DesktopError> {
        self.attach_scan_failure_recovery_recording(recording_id)?;
        let local_id = lock(&self.inner.recovered_scan_recordings)
            .get(recording_id)
            .copied()
            .ok_or_else(scan_recovery_error)?;
        let state = self
            .inner
            .scan_session(local_id)
            .ok_or_else(scan_recovery_error)?;
        Ok(ScanFailureRecovery {
            host: Arc::clone(&self.inner),
            state,
        })
    }

    /// Finalize an observed confirmation. A durable outbox verifies the
    /// authority-bound proof after rehydration; after this host has actually
    /// reclaimed that outbox, the process-bound proof permits only its exact
    /// lost-response retry.
    pub fn finalize_scan_failure_release(
        &self,
        id: u64,
        authority: &[u8; 32],
        proof: &ScanFailureCleanupProof,
    ) -> Result<(), DesktopError> {
        let state = self.inner.scan_session(id);
        let Some(state) = state else {
            self.inner
                .scan_cleanup_mac(id, authority, &proof.receipt)?
                .verify_slice(&proof.authenticator)
                .map_err(|_| scan_recovery_error())?;
            self.inner
                .reclaimed_scan_cleanup_mac(id, authority, &proof.receipt)
                .verify_slice(&proof.reclamation_authenticator)
                .map_err(|_| scan_recovery_error())?;
            return Ok(());
        };
        self.inner.authenticate_scan_authority(&state, authority)?;
        self.inner
            .scan_cleanup_mac(id, authority, &proof.receipt)?
            .verify_slice(&proof.authenticator)
            .map_err(|_| scan_recovery_error())?;
        let mut ingress = lock(&self.inner.scan_ingress);
        state
            .outbox
            .finalize_scan_failure_release(&proof.receipt)
            .map_err(|_| scan_recovery_error())?;
        if !ingress.pending_by_session.contains_key(&id)
            && state.outbox.pending_scan_failure_owners() == 0
        {
            ingress.retired_sessions.remove(&id);
            lock(&self.inner.recovered_scan_recordings)
                .retain(|_, recovered_id| *recovered_id != id);
        }
        Ok(())
    }

    /// Explicit process-owner shutdown: dispose every session, shut the
    /// central down, fail every waiting request. Returns the cleanup
    /// record JSON (`released` / `release-failed` with every failure).
    pub async fn shutdown(&self) -> String {
        let mut phase = self.inner.shutdown_owner_phase.subscribe();
        let mut updates = {
            let mut running = lock(&self.inner.shutdown_running);
            let updates = self.inner.shutdown_updates.subscribe();
            if !*running {
                *running = true;
                let host = self.clone();
                self.inner
                    .runtime
                    .spawn(async move { host.run_shutdown_owner().await });
            }
            updates
        };
        let answer = tokio::select! {
            result = updates.changed() => result.ok(),
            _ = async { let _ = phase.wait_for(|active| *active).await; } => {
                tokio::time::timeout(std::time::Duration::from_secs(1), updates.changed()).await.ok().and_then(Result::ok)
            }
        };
        if answer.is_some() {
            if let Some(receipt) = updates.borrow().clone() {
                return receipt;
            }
        }
        self.interrupted_shutdown_receipt(
            "ordered-owner",
            DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "host.shutdown.ordered-owner",
            )
            .with_detail(
                "native owner retirement remains pending; the attempt is retained until settlement",
            ),
        )
    }

    /// Observe successful autonomous shutdown. The observer must be
    /// idempotent and must release only the exact owner it represents.
    pub fn observe_shutdown_completion(&self, observer: Arc<dyn Fn(&str) + Send + Sync>) {
        let receipt = {
            let mut completion = lock(&self.inner.shutdown_completion);
            completion.observer = Some(Arc::clone(&observer));
            completion.receipt.clone()
        };
        if let Some(receipt) = receipt {
            observer(&receipt);
        }
    }

    async fn run_shutdown_owner(&self) {
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            let host = self.clone();
            // Join the attempt so a callback panic cannot kill the supervisor
            // or discard previously collected cleanup failures.
            let result = self
                .inner
                .runtime
                .spawn(async move {
                    let mut state = host.inner.shutdown_state.lock().await;
                    if let Some(receipt) = &state.complete {
                        return receipt.clone();
                    }
                    let receipt = host.shutdown_attempt(&mut state).await;
                    if crate::binding_release_succeeded(&receipt) {
                        state.complete = Some(receipt.clone());
                    }
                    receipt
                })
                .await;
            let receipt = match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    let error = DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Cleanup,
                        "host.shutdown",
                    )
                    .with_detail(format!("shutdown task failed: {error}"));
                    let failure = crate::session::cleanup_failure("host", &error);
                    let mut observed = lock(&self.inner.shutdown_observed_failures);
                    if !observed.contains(&failure) {
                        observed.push(failure);
                    }
                    drop(observed);
                    self.interrupted_shutdown_receipt("host", error)
                }
            };
            let released = crate::binding_release_succeeded(&receipt);
            let retry = {
                let state = self.inner.shutdown_state.lock().await;
                !released && state.retirement_started && state.retirement_retryable
            };
            if released {
                let observer = {
                    let mut completion = lock(&self.inner.shutdown_completion);
                    completion.receipt = Some(receipt.clone());
                    completion.observer.clone()
                };
                if let Some(observer) = observer {
                    observer(&receipt);
                }
            }
            if !retry {
                let mut running = lock(&self.inner.shutdown_running);
                self.inner.shutdown_updates.send_replace(Some(receipt));
                *running = false;
                return;
            }
            self.inner.shutdown_updates.send_replace(Some(receipt));
            tokio::select! {
                _ = tokio::time::sleep(delay) => {},
                _ = self.inner.radio.owner_settlement_changed() => {},
            }
            delay = (delay * 2).min(std::time::Duration::from_secs(30));
        }
    }

    fn interrupted_shutdown_receipt(&self, resource: &str, error: DesktopError) -> String {
        let mut failures = lock(&self.inner.shutdown_observed_failures).clone();
        for retained in self.inner.radio.v2_recovery_errors() {
            let failure = crate::session::cleanup_failure("ordered-recovery", &retained);
            if !failures.contains(&failure) {
                failures.push(failure);
            }
        }
        let failure = crate::session::cleanup_failure(resource, &error);
        if !failures.contains(&failure) {
            failures.push(failure);
        }
        crate::session::cleanup_record(failures).to_string()
    }

    #[cfg(test)]
    pub(crate) fn test_hold_scan_failure_transfer(&self, held: bool) {
        lock(&self.inner.scan_ingress).failure_transfers_in_flight = usize::from(held);
    }

    async fn shutdown_attempt(&self, state: &mut HostShutdownState) -> String {
        let receipt = self.shutdown_attempt_inner(state).await;
        let mut record: Value =
            serde_json::from_str(&receipt).expect("internal cleanup record is valid JSON");
        let failures = record["failures"]
            .as_array_mut()
            .expect("internal cleanup record has failures");
        for error in self.inner.radio.v2_recovery_errors() {
            let failure = crate::session::cleanup_failure("ordered-recovery", &error);
            if !failures.contains(&failure) {
                failures.push(failure);
            }
        }
        let mut observed = lock(&self.inner.shutdown_observed_failures);
        if failures.is_empty() {
            observed.clear();
        } else {
            for failure in failures.iter() {
                if !observed.contains(failure) {
                    observed.push(failure.clone());
                }
            }
            for failure in observed.iter() {
                if !failures.contains(failure) {
                    failures.push(failure.clone());
                }
            }
            record["state"] = Value::from("release-failed");
        }
        record.to_string()
    }

    async fn shutdown_attempt_inner(&self, state: &mut HostShutdownState) -> String {
        let inner = &*self.inner;
        let sessions = {
            let sessions = lock(&inner.sessions);
            inner.session_admission_closed.store(true, Ordering::SeqCst);
            sessions.values().cloned().collect::<Vec<_>>()
        };
        {
            let _admission = lock(&inner.continuation_admission);
            inner.continuation_closed.store(true, Ordering::SeqCst);
        }
        if let Some(executor) = inner.continuation.get() {
            executor.stop_recovery();
        }
        let mut failures = ShutdownFailures {
            values: Vec::new(),
            observed: &inner.shutdown_observed_failures,
        };
        for state in sessions {
            let session = MobileSession::new(Arc::clone(&self.inner), state);
            failures.extend(session.dispose_failures().await);
        }
        // Detached recorders are process-owned; final host shutdown drives
        // them through the same supervised fence, drain, and cleanup.
        let recordings: Vec<_> = lock(&inner.ordered_recordings)
            .iter()
            .map(|(key, owner)| (key.clone(), Arc::clone(owner)))
            .collect();
        for (key, owner) in recordings {
            if matches!(
                owner.driver_status(),
                ubm_desktop::ordered_recording::RecordingDriverStatus::Failed(_)
            ) {
                owner.retry_driver();
            }
            match owner.shutdown().await {
                Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished) => {
                    lock(&inner.ordered_recordings).remove(&key);
                    inner
                        .ordered_owners
                        .retire_host_owned(owner.collection_id(), owner.peer_id());
                }
                result => {
                    let error = DesktopError::new(
                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                        ubm_core::contracts::BleErrorDomain::Cleanup,
                        "host.shutdown.recording",
                    )
                    .with_detail("ordered recording cleanup remains pending");
                    let mut failure = crate::session::cleanup_failure("ordered-recording", &error);
                    failure["recordingReceipt"] =
                        ubm_desktop::ordered_recording::stop_receipt(result);
                    failures.push(failure);
                }
            }
        }
        if !lock(&inner.ordered_recordings).is_empty() {
            return crate::session::cleanup_record(failures.values.clone()).to_string();
        }
        let scopes: Vec<BackgroundScope> = lock(&inner.background).keys().cloned().collect();
        for scope in scopes {
            failures.extend(inner.release_background_scope(&scope).await);
        }
        {
            // Serialize the final shutdown decision with scan callback
            // admission. A captured cut or receipt keeps its host alive.
            let ingress = lock(&inner.scan_ingress);
            let sessions = lock(&inner.sessions);
            let scan_owned = !ingress.failure_transfers.is_empty()
                || ingress.failure_transfers_in_flight != 0
                || ingress
                    .retired_sessions
                    .values()
                    .chain(sessions.values())
                    .any(|session| session.outbox.pending_scan_failure_owners() != 0);
            if scan_owned {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.scan-failure",
                )
                .with_detail("scan failure ownership or receipt release remains pending");
                failures.push(crate::session::cleanup_failure("scan-failure", &error));
                return crate::session::cleanup_record(failures.values.clone()).to_string();
            }
            inner.shut_down.store(true, Ordering::SeqCst);
        }
        if let Some((coordinator, host)) = &inner.ordered_group_owner {
            let count = coordinator.host_group_count(*host);
            if count != 0 {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.ordered-admission",
                )
                .with_detail(format!("{count} ordered group(s) remain owned; native retirement or removal is unresolved"));
                failures.push(crate::session::cleanup_failure("ordered-admission", &error));
            }
            if inner.radio.uses_v2_admission() && !inner.radio.owner_retirement_supported() {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.ordered-owner",
                )
                .with_detail("native owner quiescence has not been confirmed; callback routing remains retained");
                failures.push(crate::session::cleanup_failure("ordered-owner", &error));
            }
            if !failures.is_empty() {
                failures.extend(
                    inner
                        .radio
                        .v2_recovery_errors()
                        .iter()
                        .map(|error| crate::session::cleanup_failure("ordered-recovery", error)),
                );
                // Recovery and native wake routing remain usable on retry.
                return crate::session::cleanup_record(failures.values.clone()).to_string();
            }
        }
        // Fence recovery task publication, then join every already-issued
        // recovery operation before consulting the durable central-debt ledger.
        // A failed attempt restarts the pump so the debt can make progress.
        if !state.recovery_stopped {
            let worker = lock(&inner.ordered_recovery_worker).take();
            if let Some(worker) = worker {
                inner.ordered_recovery_stop.notify_one();
                if let Err(error) = worker.await {
                    let error = DesktopError::new(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Cleanup,
                        "host.shutdown.ordered-recovery",
                    )
                    .with_detail(format!("recovery worker join failed: {error}"));
                    state
                        .worker_failures
                        .push(crate::session::cleanup_failure("ordered-recovery", &error));
                }
            }
            state.recovery_stopped = true;
        }
        if !state.worker_failures.is_empty() {
            failures.extend(state.worker_failures.iter().cloned());
            return crate::session::cleanup_record(failures.values.clone()).to_string();
        }
        let debt = inner.radio.abandoned_v2_opens();
        if !debt.is_empty() {
            if inner.radio.uses_v2_admission() {
                *lock(&inner.ordered_recovery_worker) =
                    Some(inner.runtime.spawn(ordered_recovery_pump(
                        Arc::downgrade(&self.inner),
                        Arc::clone(&inner.ordered_recovery_stop),
                    )));
                state.recovery_stopped = false;
            }
            let error = DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "host.shutdown.ordered-recovery",
            )
            .with_detail(format!(
                "{} central ordered cleanup obligation(s) remain",
                debt.len()
            ));
            failures.push(crate::session::cleanup_failure("ordered-recovery", &error));
            failures.extend(
                inner
                    .radio
                    .v2_recovery_errors()
                    .iter()
                    .map(|error| crate::session::cleanup_failure("ordered-recovery", error)),
            );
            return crate::session::cleanup_record(failures.values.clone()).to_string();
        }
        // The central may still issue native cleanup; finish it before asking
        // the native owner to prove that its work has quiesced.
        if !state.central_released {
            let report = inner.central.shutdown().await;
            state.central_released = report.is_released();
            failures.extend(shutdown_failures(report));
            if let Some(error) = inner.radio.close_error() {
                state.central_released = false;
                failures.push(crate::session::cleanup_failure("radio", &error));
            }
            if !state.central_released || !failures.is_empty() {
                let mut record = crate::session::cleanup_record(failures.values.clone());
                record["state"] = Value::from("release-failed");
                return record.to_string();
            }
        }
        // Central teardown must not have exposed another retained obligation
        // after the first debt check. Never certify this host in that case.
        let debt = inner.radio.abandoned_v2_opens();
        if !debt.is_empty() {
            if inner.radio.uses_v2_admission() {
                *lock(&inner.ordered_recovery_worker) =
                    Some(inner.runtime.spawn(ordered_recovery_pump(
                        Arc::downgrade(&self.inner),
                        Arc::clone(&inner.ordered_recovery_stop),
                    )));
                state.recovery_stopped = false;
            }
            let error = DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "host.shutdown.ordered-recovery",
            )
            .with_detail(format!(
                "{} central ordered cleanup obligation(s) remain",
                debt.len()
            ));
            failures.push(crate::session::cleanup_failure("ordered-recovery", &error));
            failures.extend(
                inner
                    .radio
                    .v2_recovery_errors()
                    .iter()
                    .map(|error| crate::session::cleanup_failure("ordered-recovery", error)),
            );
            return crate::session::cleanup_record(failures.values.clone()).to_string();
        }
        inner.ordered_wakes.close();
        if let Some(error) = inner.radio.close_error() {
            failures.push(crate::session::cleanup_failure("radio", &error));
        }
        inner.radio.close_events();
        inner.radio.abandon_pending();
        let ordered_worker = lock(&inner.ordered_pump).take();
        if let Some(worker) = ordered_worker {
            if let Err(error) = worker.await {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.ordered-pump",
                )
                .with_detail(format!("ordered pump join failed: {error}"));
                state
                    .worker_failures
                    .push(crate::session::cleanup_failure("ordered-pump", &error));
            }
        }
        inner.signals.close();
        let worker = lock(&inner.pump).take();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.pump",
                )
                .with_detail(format!("pump join failed: {error}"));
                state
                    .worker_failures
                    .push(crate::session::cleanup_failure("pump", &error));
            }
        }
        if !state.worker_failures.is_empty() {
            failures.extend(state.worker_failures.iter().cloned());
            return crate::session::cleanup_record(failures.values.clone()).to_string();
        }
        // Both terminal-producing pumps are now closed and joined. A later
        // ingress cannot append another staged debt after this exact handoff.
        for error in inner.handoff_scan_terminal_evidence_debts_to_recovery() {
            failures.push(crate::session::cleanup_failure("terminal-evidence", &error));
        }
        for error in inner.handoff_subscription_terminal_evidence_debts_to_recovery() {
            failures.push(crate::session::cleanup_failure("terminal-evidence", &error));
        }
        if !failures.is_empty() {
            let mut record = crate::session::cleanup_record(failures.values.clone());
            record["state"] = Value::from("release-failed");
            return record.to_string();
        }
        if inner.radio.uses_v2_admission() {
            if !state.local_cleanup_settled {
                if let Err(error) = self.confirm_ordered_host_cleanup() {
                    let error = DesktopError::new(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Cleanup,
                        "host.shutdown.ordered-owner",
                    )
                    .with_detail(format!("host cleanup remains retained: {error:?}"));
                    failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                    return crate::session::cleanup_record(failures.values.clone()).to_string();
                }
                state.local_cleanup_settled = true;
            }
            // The owner retains both the exact identity and the attempt lock.
            // A cancelled waiter cannot cancel its detached attempt, and a
            // different settled host can retry after that attempt has ended.
            inner.shutdown_owner_phase.send_replace(true);
            let (coordinator, _) = inner
                .ordered_group_owner
                .as_ref()
                .expect("v2 admission has a coordinator");
            let mut retirement = coordinator.retirement.lock().await;
            if let (Some(owner), Some(identity)) =
                (inner.ordered_retirement_owner, retirement.identity)
            {
                if let Some(receipt) = inner.radio.take_owner_settlement(owner, identity) {
                    retirement.attempt_unsettled = false;
                    retirement.uncertain_failure = None;
                    match receipt.state {
                        crate::OwnerRetirementState::Quiescent => {
                            retirement.native_quiescent = true
                        }
                        crate::OwnerRetirementState::FencedPending {
                            outstanding_classes,
                            failures: native_failures,
                        } => {
                            retirement.fence_confirmed = true;
                            failures.extend(pending_owner_retirement_cleanup(
                                inner.platform,
                                &outstanding_classes,
                                &native_failures,
                            ));
                            return crate::session::cleanup_record(failures.values.clone())
                                .to_string();
                        }
                        crate::OwnerRetirementState::Rejected { reason } => {
                            let error = DesktopError::new(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                "host.shutdown.ordered-owner",
                            )
                            .with_detail(format!("native owner retirement rejected: {reason:?}"));
                            failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                            return crate::session::cleanup_record(failures.values.clone())
                                .to_string();
                        }
                    }
                }
            }
            if let Some(error) = &retirement.uncertain_failure {
                failures.push(crate::session::cleanup_failure("ordered-owner", error));
                return crate::session::cleanup_record(failures.values.clone()).to_string();
            }
            if retirement.attempt_unsettled {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.ordered-owner",
                )
                .with_detail(
                    "an earlier native retirement attempt has no authoritative settlement",
                );
                failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                return crate::session::cleanup_record(failures.values.clone()).to_string();
            }
            if inner
                .ordered_group_owner
                .as_ref()
                .is_some_and(|(owner, _)| owner.native_quiescence_confirmed())
            {
                retirement.native_quiescent = true;
            } else {
                if inner.radio.uses_v2_admission() {
                    if let Err(error) = inner
                        .radio
                        .begin_exclusive_owner_retirement(Arc::clone(&self.inner))
                    {
                        let error = DesktopError::new(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "host.shutdown.ordered-owner",
                        )
                        .with_detail(format!(
                            "owner retirement cannot fence another live host: {error:?}"
                        ));
                        failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                        return crate::session::cleanup_record(failures.values.clone()).to_string();
                    }
                    state.retirement_started = true;
                    state.retirement_retryable = true;
                }
                if inner.radio.uses_v2_admission() && !retirement.native_quiescent {
                    let Some(owner) = inner.ordered_retirement_owner else {
                        let error = DesktopError::new(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "host.shutdown.ordered-owner",
                        )
                        .with_detail(
                            "native owner retirement version was negotiated without an owner identity",
                        );
                        failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                        return crate::session::cleanup_record(failures.values.clone()).to_string();
                    };
                    if retirement.identity.is_none() {
                        let mut bytes = [0u8; 16];
                        if getrandom::fill(&mut bytes).is_err() {
                            let error = DesktopError::new(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                "host.shutdown.ordered-owner",
                            )
                            .with_detail("could not allocate retirement identity");
                            failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                            return crate::session::cleanup_record(failures.values.clone())
                                .to_string();
                        }
                        bytes[6] = (bytes[6] & 0x0f) | 0x40;
                        bytes[8] = (bytes[8] & 0x3f) | 0x80;
                        let s = format!(
                            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                            bytes[0],
                            bytes[1],
                            bytes[2],
                            bytes[3],
                            bytes[4],
                            bytes[5],
                            bytes[6],
                            bytes[7],
                            bytes[8],
                            bytes[9],
                            bytes[10],
                            bytes[11],
                            bytes[12],
                            bytes[13],
                            bytes[14],
                            bytes[15]
                        );
                        retirement.identity =
                            crate::OrderedAdmissionOwner::negotiate_slot(2, &s).ok();
                    }
                    let retirement_id = retirement.identity.expect("generated UUID is valid");
                    let fenced = retirement.fence_confirmed;
                    // A timeout or caller cancellation is not native settlement.
                    // Retain the request and the shared owner lock until an answer.
                    retirement.attempt_unsettled = true;
                    let mut request_id = None;
                    let result = inner
                        .radio
                        .call(|id| {
                            request_id = Some(id);
                            if fenced {
                                crate::RadioRequest::OwnerStatus {
                                    id,
                                    owner_incarnation: owner,
                                    retirement_id,
                                }
                            } else {
                                crate::RadioRequest::OwnerFence {
                                    id,
                                    owner_incarnation: owner,
                                    retirement_id,
                                }
                            }
                        })
                        .await;
                    let pending = match result {
                        Ok(RadioCompletion::OwnerRetirement(receipt)) => {
                            if let Some(id) = request_id {
                                inner.radio.forget_owner_attempt(id);
                            }
                            retirement.attempt_unsettled = false;
                            match receipt.state {
                                crate::OwnerRetirementState::Quiescent => {
                                    retirement.native_quiescent = true;
                                    None
                                }
                                crate::OwnerRetirementState::FencedPending {
                                    outstanding_classes,
                                    failures,
                                } => {
                                    retirement.fence_confirmed = true;
                                    Some((outstanding_classes, failures))
                                }
                                crate::OwnerRetirementState::Rejected { reason } => {
                                    let error = DesktopError::new(
                                        BleErrorCode::LifecycleInvalidState,
                                        BleErrorDomain::Cleanup,
                                        "host.shutdown.ordered-owner",
                                    )
                                    .with_detail(format!(
                                        "native owner retirement rejected: {reason:?}"
                                    ));
                                    failures.push(crate::session::cleanup_failure(
                                        "ordered-owner",
                                        &error,
                                    ));
                                    return crate::session::cleanup_record(failures.values.clone())
                                        .to_string();
                                }
                            }
                        }
                        Ok(other) => {
                            let error = DesktopError::new(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                "host.shutdown.ordered-owner",
                            )
                            .with_detail(format!(
                                "native settlement is unknown after unexpected response: {other:?}"
                            ));
                            retirement.uncertain_failure = Some(error.clone());
                            let error = DesktopError::new(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                "host.shutdown.ordered-owner",
                            )
                            .with_detail(format!(
                                "native settlement is unknown after unexpected response: {other:?}"
                            ));
                            failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                            return crate::session::cleanup_record(failures.values.clone())
                                .to_string();
                        }
                        Err(error) => {
                            if error.commit()
                                == Some(ubm_core::contracts::CommitState::NotDispatched)
                            {
                                if let Some(id) = request_id {
                                    inner.radio.forget_owner_attempt(id);
                                }
                                retirement.attempt_unsettled = false;
                            } else {
                                retirement.uncertain_failure = Some(error.clone());
                            }
                            failures.push(crate::session::cleanup_failure("ordered-owner", &error));
                            return crate::session::cleanup_record(failures.values.clone())
                                .to_string();
                        }
                    };
                    if let Some((outstanding_classes, native_failures)) = pending {
                        failures.extend(pending_owner_retirement_cleanup(
                            inner.platform,
                            &outstanding_classes,
                            &native_failures,
                        ));
                        return crate::session::cleanup_record(failures.values.clone()).to_string();
                    }
                }
            }
            let retired = if inner
                .ordered_group_owner
                .as_ref()
                .is_some_and(|(owner, _)| owner.native_quiescence_confirmed())
            {
                Ok(())
            } else {
                self.retire_ordered_owner_after_native_quiescence()
            };
            if let Err(error) = retired {
                let error = DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "host.shutdown.ordered-owner",
                )
                .with_detail(format!("owner cleanup remains retained: {error:?}"));
                failures.push(crate::session::cleanup_failure("ordered-owner", &error));
            }
        }
        let mut record = crate::session::cleanup_record(failures.values.clone());
        if !state.central_released {
            record["state"] = Value::from("release-failed");
        }
        record.to_string()
    }

    /// End a shared background scope (React Native module invalidation):
    /// release every foreground-service lease its sessions acquired.
    /// Returns the cleanup record JSON; a lease whose release failed stays
    /// held and a second call retries it.
    pub async fn release_background_scope(&self, scope: &str) -> String {
        let failures = self
            .inner
            .release_background_scope(&BackgroundScope::Shared(scope.to_owned()))
            .await;
        crate::session::cleanup_record(failures).to_string()
    }

    /// Blocking form of [`Self::release_background_scope`] for native
    /// threads.
    #[must_use]
    pub fn release_background_scope_blocking(&self, scope: &str) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = self.clone();
        let scope = scope.to_owned();
        self.inner
            .runtime
            .spawn(async move { host.release_background_scope(&scope).await }.then_send(tx));
        rx.recv().unwrap_or_else(|_| {
            crate::session::cleanup_record(vec![crate::session::cleanup_failure(
                "background",
                &DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "ubm-mobile.host.release-background-scope",
                ),
            )])
            .to_string()
        })
    }

    /// Blocking form of [`Self::shutdown`] for native threads.
    #[must_use]
    pub fn shutdown_blocking(&self) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = self.clone();
        self.inner
            .runtime
            .spawn(async move { host.shutdown().await }.then_send(tx));
        rx.recv().unwrap_or_else(|_| {
            crate::session::cleanup_record(vec![crate::session::cleanup_failure(
                "central",
                &DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "ubm-mobile.host.shutdown",
                ),
            )])
            .to_string()
        })
    }

    /// Radio counters (tests and diagnostics).
    #[must_use]
    pub fn radio_counters(&self) -> crate::foreign::RadioCounters {
        self.inner.radio.counters()
    }
}

/// Send a future's output over a std channel when it completes.
trait ThenSend: std::future::Future + Sized {
    fn then_send(
        self,
        tx: std::sync::mpsc::Sender<Self::Output>,
    ) -> impl std::future::Future<Output = ()> + Send
    where
        Self: Send,
        Self::Output: Send,
    {
        async move {
            let _ = tx.send(self.await);
        }
    }
}

impl<F: std::future::Future> ThenSend for F {}

fn shutdown_failures(report: ubm_desktop::ShutdownReport) -> Vec<Value> {
    let mut failures = Vec::new();
    for failure in report.radio_close_failures {
        failures.push(crate::session::cleanup_failure(
            "subscription",
            &DesktopError::new(
                BleErrorCode::GattSubscribeFailed,
                BleErrorDomain::Cleanup,
                "radio.close",
            )
            .with_detail(failure.detail),
        ));
    }
    for error in report.half_open_close_failures {
        failures.push(crate::session::cleanup_failure("connection", &error));
    }
    for error in report.transport_close_failures {
        failures.push(crate::session::cleanup_failure("backend", &error));
    }
    if let Some(error) = report.scan_stop_failure {
        failures.push(crate::session::cleanup_failure("scan", &error));
    }
    match report.record {
        Ok(record) => {
            for failure in record.failures() {
                failures.push(crate::session::cleanup_failure(
                    failure.resource_kind(),
                    &DesktopError::new(failure.code(), BleErrorDomain::Cleanup, "central.destroy"),
                ));
            }
        }
        Err(error) => failures.push(crate::session::cleanup_failure("central", &error)),
    }
    failures
}

#[cfg(test)]
#[path = "source_failure_ingress_tests.rs"]
mod source_failure_ingress_tests;

#[cfg(test)]
mod signal_tests {
    #[test]
    fn shutdown_observer_registration_racing_a_completed_receipt_observes_once() {
        use crate::test_common::*;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let radio = Scripted::polar();
        let (host, _) = runtime.block_on(open(&radio, crate::MobilePlatform::Android));
        let observed = Arc::new(AtomicUsize::new(0));
        let observed_by_callback = Arc::clone(&observed);
        let mut completion = super::lock(&host.inner.shutdown_completion);
        let registering = {
            let host = host.clone();
            std::thread::spawn(move || {
                host.observe_shutdown_completion(Arc::new(move |receipt| {
                    assert_eq!(receipt, r#"{\"state\":\"released\",\"failures\":[]}"#);
                    observed_by_callback.fetch_add(1, Ordering::SeqCst);
                }));
            })
        };
        completion.receipt = Some(r#"{\"state\":\"released\",\"failures\":[]}"#.to_owned());
        drop(completion);
        registering.join().unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_pump_join_is_retained_across_shutdown_retries() {
        use crate::test_common::*;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let original = super::lock(&host.inner.ordered_pump).take().unwrap();
        original.abort();
        let _ = original.await;
        let failed = tokio::spawn(async { panic!("injected ordered pump failure") });
        *super::lock(&host.inner.ordered_pump) = Some(failed);
        for _ in 0..2 {
            let receipt: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
            assert_eq!(receipt["state"], "release-failed");
            assert!(receipt["failures"].to_string().contains("ordered-pump"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_failure_exits_while_shutdown_attempt_holds_its_lock() {
        use crate::test_common::*;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let guard = host.inner.shutdown_state.lock().await;
        let owner = host.inner.clone();
        let failure = tokio::spawn(async move {
            owner
                .fail_pump(serde_json::json!({"operation":"test.failure"}))
                .await;
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), failure).await;
        drop(guard);
        assert!(result.is_ok(), "the pump waited for the shutdown lock");
        assert!(host.ingress_failure().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_source_failure_closes_owner_when_control_is_saturated() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("source-overflow").unwrap();
        for (operation, id) in [
            ("connection.connect", "connect"),
            ("gatt.discover", "discover"),
        ] {
            ok(&call(
                &session,
                operation,
                &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
            )
            .await);
        }
        ok(&call(&session, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"subscribe"}).to_string()).await);
        let epoch = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .find_map(|request| match request {
                crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .unwrap();
        let fence = host.inner.radio.fence_ingress().await.unwrap();
        for _ in 0..crate::foreign::CONTROL_INGRESS_CAP {
            host.inner
                .radio
                .push_event(ubm_desktop::RadioEvent::EventsLost { skipped: 1 })
                .unwrap();
        }
        let instance = crate::Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        };
        assert!(matches!(
            host.ingest(crate::RadioIngress::NotificationSourceFailed {
                instance,
                epoch,
                failure: crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "source stopped"
                )
            }),
            crate::IngressStatus::Dropped(crate::IngressClass::Control)
        ));
        assert!(host.inner.is_shut_down());
        drop(fence);
        let records = drain_until(&session, |records| {
            !of_type(records, "stream-end").is_empty()
        })
        .await;
        assert_eq!(
            of_type(&records, "stream-end")[0]["reason"],
            "source-failed"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while host.ingress_failure().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recorded_unsubscribe_later_slot_cannot_hold_the_ingress_fence() {
        use crate::test_common::*;
        use serde_json::json;
        use std::future::Future;
        use std::task::Poll;
        use ubm_desktop::OpControl;

        let Some(directory) = recording_test_directory(
            "host::signal_tests::recorded_unsubscribe_later_slot_cannot_hold_the_ingress_fence",
        ) else {
            return;
        };
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        host.continuation()
            .configure_recording_directory(&directory)
            .unwrap();
        let session = host.open_session("reverse-admission").unwrap();
        for (operation, id) in [
            ("connection.connect", "connect"),
            ("gatt.discover", "discover"),
        ] {
            ok(&call(
                &session,
                operation,
                &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
            )
            .await);
        }
        ok(&call(&session, "gatt.subscribe-recorded", &json!({"peerId":POLAR,"selector":selector(),"consumer":"recorded","operationId":"subscribe","recording":{"id":"reverse","maxBytes":1048576,"maxRecords":1000}}).to_string()).await);
        let epoch = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .find_map(|request| match request {
                crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .unwrap();
        let (scope, route) = {
            let routes = super::lock(&host.inner.routes);
            let (scope, routes) = routes
                .iter()
                .find(|(_, routes)| routes.iter().any(|route| route.consumer == "recorded"))
                .unwrap();
            (
                scope.clone(),
                routes
                    .iter()
                    .find(|route| route.consumer == "recorded")
                    .unwrap()
                    .clone(),
            )
        };
        let first = OpControl::budget_ms(3000)
            .with_gatt_admission(host.inner.central.admit_gatt(POLAR).unwrap());
        let second = OpControl::budget_ms(3000)
            .with_gatt_admission(host.inner.central.admit_gatt(POLAR).unwrap());
        let order = &route.recorded.as_ref().unwrap().order;
        let held_order = order.lock().await;
        let later = host.inner.unsubscribe_recorded(
            &scope,
            route.session_id,
            &route.consumer,
            &route.core_consumer,
            &route.selector,
            second,
        );
        tokio::pin!(later);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(later.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert!(!host.recorded_ingress_paused());
        drop(held_order);
        host.ingest(crate::RadioIngress::Notification {
            instance: crate::Instance {
                peer_id: POLAR.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch,
            value: vec![42],
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.inner.unsubscribe_recorded(
                &scope,
                route.session_id,
                &route.consumer,
                &route.core_consumer,
                &route.selector,
                first,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), later)
            .await
            .unwrap()
            .unwrap();
        let batch = ok(&host.continuation_recording_prepare("reverse", 100, 65536));
        assert!(
            batch["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["record"]["valueB64"] == "Kg==")
        );
        host.shutdown().await;
    }

    #[test]
    fn native_receipt_utc_preserves_signed_fractional_milliseconds() {
        assert_eq!(
            super::utc_ms(std::time::UNIX_EPOCH + std::time::Duration::from_micros(1250)),
            1.25
        );
        assert_eq!(
            super::utc_ms(std::time::UNIX_EPOCH - std::time::Duration::from_micros(1250)),
            -1.25
        );
    }

    #[test]
    fn scan_failure_has_reserved_ingress_capacity() {
        let signals = super::Signals::default();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            signals.push(lifecycle(sequence));
        }
        let failure = |detail| {
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::ScanStartFailed,
                ubm_core::contracts::BleErrorDomain::Scan,
                "scan.start",
            )
            .with_detail(detail)
        };
        assert_eq!(
            signals.push_scan_failure(
                failure("scan failure"),
                42,
                Vec::new(),
                Arc::new(()),
                "radio"
            ),
            super::ScanFailureAdmission::Queued
        );
        assert_eq!(
            signals.push_scan_failure(
                failure("another failure"),
                43,
                Vec::new(),
                Arc::new(()),
                "radio"
            ),
            super::ScanFailureAdmission::Emergency
        );
        assert_eq!(
            signals.push_scan_failure(
                failure("excess failure"),
                44,
                Vec::new(),
                Arc::new(()),
                "radio"
            ),
            super::ScanFailureAdmission::Closed
        );
        let mut found = Vec::new();
        while let Some(signal) = signals.pop() {
            if let super::HostSignal::ScanFailedScoped(error, generation, _, _, _) = signal {
                found.push((generation, error.detail().unwrap().to_owned()));
            }
        }
        assert_eq!(
            found,
            vec![
                (42, "scan failure".to_owned()),
                (43, "another failure".to_owned())
            ]
        );
    }

    fn assert_saturated_scan_signal_returns_the_original_for_retry() {
        use ubm_desktop::errors::{PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let signals = super::Signals::default();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            signals.push(lifecycle(sequence));
        }
        let failure = || {
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::ScanStartFailed,
                ubm_core::contracts::BleErrorDomain::Scan,
                "scan.start",
            )
        };
        for generation in [1, 2] {
            assert!(
                signals
                    .try_push_scan_failure(failure(), generation, Vec::new(), Arc::new(()), "radio")
                    .is_ok()
            );
        }
        let mut platform = PlatformDetail::new("radio", "failure");
        platform.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "duplicate".into(),
                value: PlatformValue::Text("first".into()),
            },
            PlatformMetadataEntry {
                key: "duplicate".into(),
                value: PlatformValue::Text("second".into()),
            },
        ]);
        platform.cleanup_failures.push(
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::BackendReset,
                ubm_core::contracts::BleErrorDomain::Core,
                "cleanup",
            )
            .with_detail("nested failure"),
        );
        let original = failure().with_platform(platform);
        let expected = original.clone();
        let refused = signals
            .try_push_scan_failure(original, 3, Vec::new(), Arc::new(()), "radio")
            .unwrap_err();
        assert_eq!(refused, expected);
        while !matches!(signals.pop(), Some(super::HostSignal::ScanFailedScoped(..))) {}
        signals.finish_scan_failure();
        assert!(
            signals
                .try_push_scan_failure(refused, 3, Vec::new(), Arc::new(()), "radio")
                .is_ok()
        );
        let mut retried = None;
        while let Some(signal) = signals.pop() {
            if let super::HostSignal::ScanFailedScoped(error, 3, _, _, _) = signal {
                retried = Some(error);
            }
        }
        assert_eq!(retried, Some(expected));
    }

    #[test]
    fn saturated_scan_signal_returns_the_original_for_retry() {
        assert_saturated_scan_signal_returns_the_original_for_retry();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_scan_ingress_reports_closed_and_keeps_the_error() {
        use crate::test_common::*;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let _ = host.shutdown().await;
        let status = host.ingest(crate::RadioIngress::ScanFailed {
            detail: "closed failure".into(),
        });
        assert_eq!(status.as_str(), "closed");
        assert!(format!("{status:?}").contains("closed failure"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_scan_outbox_transfer_is_retried_without_new_ingress() {
        use crate::test_common::*;
        use ubm_core::oversized_evidence::{EvidenceSource, EvidenceToken, ScanFailureOccurrence};
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("retry-scan-transfer").unwrap();
        let state = host.inner.scan_session(session.id()).unwrap();
        *super::lock(&state.scan_end_version) = Some(3);
        ok(&call(
            &session,
            "scan.start",
            &serde_json::json!({
                "serviceUuids": [], "duplicatePolicy": "all", "operationId": "scan"
            })
            .to_string(),
        )
        .await);
        let pump = super::lock(&host.inner.pump).take().unwrap();
        pump.abort();
        let _ = pump.await;
        let failure = ubm_desktop::DesktopError::new(
            ubm_core::contracts::BleErrorCode::ScanStartFailed,
            ubm_core::contracts::BleErrorDomain::Scan,
            "scan.start",
        )
        .with_detail("original");
        host.ingest_scan_failure_owned(failure, None).unwrap();
        let original = {
            let mut ingress = super::lock(&host.inner.scan_ingress);
            let transfer = ingress
                .failure_transfers
                .values_mut()
                .next()
                .unwrap()
                .first_mut()
                .unwrap();
            let original = transfer.occurrence.take().unwrap();
            let oversized = ScanFailureOccurrence::new(
                original.identity(),
                original.members().to_vec(),
                Arc::new(EvidenceSource::new(vec![EvidenceToken::Text(
                    "x".repeat(1024 * 1024),
                )])),
            );
            transfer.occurrence = Some(oversized);
            original
        };
        while let Some(signal) = host.inner.signals.pop() {
            let failure = matches!(signal, super::HostSignal::ScanFailedScoped(..));
            host.inner.handle(signal).await;
            if failure {
                break;
            }
        }
        {
            let mut ingress = super::lock(&host.inner.scan_ingress);
            assert_eq!(ingress.deferred_failures.len(), 1);
            let transfer = ingress
                .failure_transfers
                .values_mut()
                .next()
                .unwrap()
                .first_mut()
                .unwrap();
            transfer.occurrence = Some(original);
        }
        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(signal) = host.inner.signals.pop() {
                    if matches!(signal, super::HostSignal::ScanFailedScoped(..)) {
                        break signal;
                    }
                    host.inner.handle(signal).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .expect("refused transfer must schedule its own retry");
        host.inner.handle(signal).await;
        assert_eq!(state.outbox.pending_scan_failure_owners(), 1);
        assert!(
            super::lock(&host.inner.scan_ingress)
                .deferred_failures
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn captured_unprepared_scan_failure_can_be_retired_after_raw_export() {
        use crate::test_common::*;
        use ubm_core::oversized_evidence::{EvidenceSource, EvidenceToken, ScanFailureOccurrence};
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("unprepared-scan").unwrap();
        let state = host.inner.scan_session(session.id()).unwrap();
        *super::lock(&state.scan_end_version) = Some(3);
        ok(&call(
            &session,
            "scan.start",
            &serde_json::json!({
                "serviceUuids": [], "duplicatePolicy": "all", "operationId": "scan"
            })
            .to_string(),
        )
        .await);
        let pump = super::lock(&host.inner.pump).take().unwrap();
        pump.abort();
        let _ = pump.await;
        let _ = session.drain(256, 65536);
        host.ingest_scan_failure_owned(
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::ScanStartFailed,
                ubm_core::contracts::BleErrorDomain::Scan,
                "scan.start",
            ),
            None,
        )
        .unwrap();
        {
            let mut ingress = super::lock(&host.inner.scan_ingress);
            let transfer = ingress
                .failure_transfers
                .values_mut()
                .next()
                .unwrap()
                .first_mut()
                .unwrap();
            let original = transfer.occurrence.take().unwrap();
            transfer.occurrence = Some(ScanFailureOccurrence::new(
                original.identity(),
                original.members().to_vec(),
                Arc::new(EvidenceSource::new(vec![EvidenceToken::Array(3)])),
            ));
        }
        while let Some(signal) = host.inner.signals.pop() {
            let failure = matches!(signal, super::HostSignal::ScanFailedScoped(..));
            host.inner.handle(signal).await;
            if failure {
                break;
            }
        }
        let pending = state
            .outbox
            .detached_scan_failure()
            .expect("raw source remains discoverable");
        assert_eq!(
            state
                .outbox
                .inspect_scan_failure(&pending)
                .unwrap()
                .token_count,
            1
        );
        state
            .outbox
            .retire_unprepared_scan_failure(&pending)
            .expect("producer disposition permits raw retirement");
    }

    // The retained-source route is exercised internally without negotiating it
    // through the current React Native bridge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_recovery_authenticates_disposed_owner_and_replays_final_cleanup() {
        use crate::test_common::*;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("scan-owner").unwrap();
        let other = host.open_session("other-owner").unwrap();
        let authority = session.scan_failure_recovery_authority().unwrap();
        let wrong = other.scan_failure_recovery_authority().unwrap();
        let state = host.inner.scan_session(session.id()).unwrap();
        *super::lock(&state.scan_end_version) = Some(3);
        ok(&call(
            &session,
            "scan.start",
            &serde_json::json!({
                "serviceUuids": [], "duplicatePolicy": "all", "operationId": "scan"
            })
            .to_string(),
        )
        .await);
        let _ = session.drain(256, 65536);
        host.ingest_scan_failure_owned(
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::ScanStartFailed,
                ubm_core::contracts::BleErrorDomain::Scan,
                "scan.start",
            )
            .with_detail("recover me"),
            None,
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while state.outbox.pending_scan_failure_owners() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(host.scan_failure_recovery(session.id(), &wrong).is_err());
        ok(&call(&session, "session.dispose", "{}").await);
        assert!(host.session(session.id()).is_none());
        let recovery = host
            .scan_failure_recovery(session.id(), &authority)
            .unwrap();
        let owners = recovery.owners();
        assert_eq!(owners.len(), 1);
        let member = recovery.member(&owners[0].key, 0).unwrap();
        let head = recovery.head(member).unwrap().unwrap();
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            let page = recovery.read(&head, offset).unwrap();
            assert_eq!(page, recovery.read(&head, offset).unwrap());
            offset = page.next_offset;
        }
        recovery.acknowledge(&head).unwrap();
        recovery.acknowledge(&head).unwrap();
        recovery.retire(&head).unwrap();
        recovery.retire(&head).unwrap();
        recovery.release(&head).unwrap();
        recovery.release(&head).unwrap();
        let proof = recovery.confirm_release(&head).unwrap();
        assert_eq!(proof, recovery.confirm_release(&head).unwrap());
        assert!(
            host.inner.scan_session(session.id()).is_some(),
            "confirmation retains a disposed outbox until finalization"
        );
        let shutdown: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert_eq!(
            shutdown["state"], "release-failed",
            "shutdown remains blocked until the confirmed cleanup is finalized"
        );
        assert!(host.inner.scan_session(session.id()).is_some());
        host.finalize_scan_failure_release(session.id(), &authority, &proof)
            .unwrap();
        assert!(host.inner.scan_session(session.id()).is_none());
        host.finalize_scan_failure_release(session.id(), &authority, &proof)
            .unwrap();
        let mut altered_proof = proof;
        altered_proof.authenticator[0] ^= 1;
        assert!(
            host.finalize_scan_failure_release(session.id(), &authority, &altered_proof)
                .is_err()
        );
        let mut altered_reclamation_proof = proof;
        altered_reclamation_proof.reclamation_authenticator[0] ^= 1;
        assert!(
            host.finalize_scan_failure_release(
                session.id(),
                &authority,
                &altered_reclamation_proof
            )
            .is_err(),
            "a reclaimed owner still requires this host's exact retry proof"
        );
        let mut altered_receipt = proof;
        altered_receipt.receipt.authenticator[0] ^= 1;
        assert!(
            host.finalize_scan_failure_release(session.id(), &authority, &altered_receipt)
                .is_err()
        );
        assert!(
            host.finalize_scan_failure_release(session.id(), &wrong, &proof)
                .is_err()
        );
        let foreign_radio = Scripted::polar();
        let (foreign_host, _) = open(&foreign_radio, crate::MobilePlatform::Android).await;
        assert!(
            foreign_host
                .finalize_scan_failure_release(session.id(), &authority, &proof)
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_failure_cut_owns_the_original_backend_failure_before_wire_drain() {
        use crate::test_common::*;
        use serde_json::json;
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        use ubm_desktop::errors::{PlatformMetadataEntry, PlatformValue};

        assert_saturated_scan_signal_returns_the_original_for_retry();

        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("scan-source-cut-522").unwrap();
        // Exercise the future route without advertising it through session
        // negotiation before the React Native transfer is complete.
        let state = host.inner.scan_session(session.id()).unwrap();
        *super::lock(&state.scan_end_version) = Some(3);
        let mut platform = ubm_desktop::PlatformDetail::new("test-radio", "source-failure");
        platform.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "duplicate".into(),
                value: PlatformValue::Text("first".into()),
            },
            PlatformMetadataEntry {
                key: "duplicate".into(),
                value: PlatformValue::Text("second".into()),
            },
        ]);
        platform.cleanup_failures.push(
            ubm_desktop::DesktopError::new(
                BleErrorCode::BackendReset,
                BleErrorDomain::Core,
                "cleanup",
            )
            .with_detail("nested failure"),
        );
        let failure = ubm_desktop::DesktopError::new(
            BleErrorCode::ScanStartFailed,
            BleErrorDomain::Scan,
            "scan.start",
        )
        .with_detail("failure ".repeat(150))
        .with_platform(platform);
        let expected = failure.clone();
        let refused = host.ingest_scan_failure_owned(failure, None).unwrap_err();
        let reconstructed = ubm_desktop::DesktopError::from(
            ubm_desktop::errors::DesktopErrorEvidence::from(refused),
        );
        assert_eq!(
            reconstructed, expected,
            "refusal and reconstruction retain both duplicate entries in order"
        );
        let tokens =
            ubm_desktop::errors::DesktopErrorEvidence::from(reconstructed.clone()).into_tokens();
        use ubm_core::oversized_evidence::EvidenceToken as Token;
        assert!(tokens.windows(6).any(|pair| matches!(pair,
            [Token::Array(2), Token::Text(key1), Token::Text(first), Token::Array(2), Token::Text(key2), Token::Text(second)]
            if key1 == "duplicate" && first == "first" && key2 == "duplicate" && second == "second"
        )), "the source tokens preserve both entries and their order");
        let started = ok(&call(
            &session,
            "scan.start",
            &json!({
                "serviceUuids": [], "duplicatePolicy": "all", "operationId": "scan"
            })
            .to_string(),
        )
        .await);
        // Drive the pump explicitly: a correct transfer must route this
        // accepted advertisement before assigning the failure's outbox cut.
        let pump = super::lock(&host.inner.pump).take().unwrap();
        pump.abort();
        let _ = pump.await;
        let _ = session.drain(256, 65536);
        assert_eq!(
            host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
                peer_id: "before-failure".into(),
                ..crate::Advertisement::default()
            })),
            crate::IngressStatus::Accepted
        );
        assert!(host.ingest_scan_failure_owned(reconstructed, None).is_ok());
        let (member, generation) = {
            let ingress = super::lock(&host.inner.scan_ingress);
            let transfer = ingress
                .failure_transfers
                .values()
                .next()
                .unwrap()
                .first()
                .unwrap();
            (
                transfer.occurrence.as_ref().unwrap().members()[0],
                ingress.generation.unwrap(),
            )
        };
        while let Some(signal) = host.inner.signals.pop() {
            let is_failure = matches!(signal, super::HostSignal::ScanFailedScoped(..));
            host.inner.handle(signal).await;
            if is_failure {
                break;
            }
        }

        let owners = state.outbox.pending_scan_failure_owners();
        host.inner.remove_session(session.id());
        assert!(
            host.inner.scan_session(session.id()).is_some(),
            "a disposed session must retain its captured outbox until receipt cleanup"
        );
        let shutdown: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert_eq!(
            shutdown["state"], "release-failed",
            "shutdown must retain outstanding scan receipts"
        );
        assert!(
            state.outbox.scan_failure_head(member).unwrap().is_none(),
            "prefix precedes the failure fence"
        );
        state.outbox.push_control(json!({"t":"after-failure"}));
        let prefix = state.outbox.drain(256, 65536);
        let records = prefix["records"].as_array().expect("prefix records");
        assert!(
            records
                .iter()
                .any(|record| record["t"] == "adv" && record["peerId"] == "before-failure"),
            "the failure cut must follow the accepted advertisement: {records:?}"
        );
        assert_eq!(
            owners, 1,
            "the original failure and duplicate metadata must be owned before destructive drain"
        );
        let head = state.outbox.scan_failure_head(member).unwrap().unwrap();
        let mut bytes = Vec::new();
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            let chunk = state.outbox.read_scan_failure(&head, offset).unwrap();
            bytes.extend_from_slice(&chunk.bytes);
            offset = chunk.next_offset;
        }
        fn decode(input: &mut &[u8]) -> serde_json::Value {
            fn number(input: &mut &[u8]) -> u64 {
                let (bytes, rest) = input.split_at(8);
                *input = rest;
                u64::from_be_bytes(bytes.try_into().unwrap())
            }
            let (tag, rest) = input.split_first().unwrap();
            *input = rest;
            match tag {
                0 => serde_json::Value::Null,
                1 => false.into(),
                2 => true.into(),
                3 => (number(input) as i64).into(),
                4 => number(input).into(),
                6 => {
                    let length = number(input) as usize;
                    let (text, rest) = input.split_at(length);
                    *input = rest;
                    std::str::from_utf8(text).unwrap().into()
                }
                9 => (0..number(input)).map(|_| decode(input)).collect(),
                _ => panic!("unexpected token tag {tag}"),
            }
        }
        let mut input = bytes.as_slice();
        let evidence = decode(&mut input);
        assert!(input.is_empty());
        assert_eq!(evidence[0][0], expected.code().as_str());
        assert_eq!(evidence[0][1], expected.domain().as_str());
        assert_eq!(evidence[0][2], expected.operation());
        assert_eq!(evidence[0][3], expected.detail().unwrap());
        assert_eq!(evidence[0][5], serde_json::Value::Null);
        assert_eq!(evidence[0][6], expected.retryability().as_str());
        assert_eq!(evidence[0][4][0], "test-radio");
        assert_eq!(evidence[0][4][1], "source-failure");
        assert_eq!(evidence[0][4][2], serde_json::Value::Null);
        assert_eq!(evidence[0][4][3], json!([]));
        assert_eq!(
            evidence[0][4][4],
            json!([["duplicate", "first"], ["duplicate", "second"]])
        );
        assert_eq!(evidence[0][4][5], json!([]));
        assert_eq!(evidence[0][4][6].as_array().unwrap().len(), 1);
        assert_eq!(evidence[0][4][6][0][0], BleErrorCode::BackendReset.as_str());
        assert_eq!(evidence[0][4][6][0][1], BleErrorDomain::Core.as_str());
        assert_eq!(evidence[0][4][6][0][2], "cleanup");
        assert_eq!(evidence[0][4][6][0][3], "nested failure");
        assert_eq!(evidence[0][4][6][0][4], serde_json::Value::Null);
        assert_eq!(evidence[0][4][6][0][5], serde_json::Value::Null);
        assert_eq!(evidence[0][4][6][0][6], "never");
        assert_eq!(evidence[1], "radio");
        assert_eq!(
            evidence[2],
            json!([generation, started["operationId"], "scan"])
        );
        assert_eq!(
            state.outbox.drain(256, 65536)["error"],
            "retained-scan-failure"
        );
        state.outbox.acknowledge_scan_failure(&head).unwrap();
        state.outbox.retire_scan_failure(&head).unwrap();
        state.outbox.release_scan_failure_receipt(&head).unwrap();
        let receipt = state
            .outbox
            .confirm_scan_failure_receipt_release(&head)
            .unwrap();
        state
            .outbox
            .finalize_scan_failure_release(&receipt)
            .unwrap();
        assert_eq!(
            state.outbox.drain(256, 65536)["records"][0]["t"],
            "after-failure"
        );
        let _ = host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v3_scan_admission_refusal_preserves_error_and_callback_recipients() {
        use crate::test_common::*;
        use serde_json::json;
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        use ubm_core::oversized_evidence::{EvidenceLimits, ScanFailureLimits};

        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let first = host.open_session("v3-first").unwrap();
        let second = host.open_session("v3-second").unwrap();
        let first_state = host.inner.scan_session(first.id()).unwrap();
        let second_state = host.inner.scan_session(second.id()).unwrap();
        *super::lock(&first_state.scan_end_version) = Some(3);
        *super::lock(&second_state.scan_end_version) = Some(3);
        let started = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        ok(&call(
            &second,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"second"}).to_string(),
        )
        .await);
        let pump = super::lock(&host.inner.pump).take().unwrap();
        pump.abort();
        let _ = pump.await;
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 1,
                max_tokens: 1,
                max_encoded_bytes: 1,
                max_chunks: 1,
                chunk_bytes: 1,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: 1,
        };
        let reservations: Vec<_> = (0..ubm_desktop::continuation_outbox::SCAN_FAILURE_OWNER_CAP)
            .map(|_| second_state.outbox.reserve_scan_failure(limits).unwrap())
            .collect();
        let ordinary = host.ingest(crate::RadioIngress::ScanFailed {
            detail: "ordinary exact refusal".into(),
        });
        assert_eq!(
            ordinary,
            crate::IngressStatus::ScanFailureRefused {
                error: ubm_desktop::DesktopError::new(
                    BleErrorCode::ScanStartFailed,
                    BleErrorDomain::Scan,
                    "scan.start"
                )
                .with_detail("ordinary exact refusal"),
                closed: false,
            }
        );
        let failure = ubm_desktop::DesktopError::new(
            BleErrorCode::ScanStartFailed,
            BleErrorDomain::Scan,
            "scan.start",
        )
        .with_detail("exact refusal");
        let refused = host
            .ingest_scan_failure_owned(failure.clone(), None)
            .unwrap_err();
        assert_eq!(refused, failure);
        {
            let ingress = super::lock(&host.inner.scan_ingress);
            assert!(ingress.failure_transfers.is_empty());
            assert_ne!(ingress.failed_generation, ingress.generation);
        }
        drop(reservations);
        host.ingest_scan_failure_owned(refused, None).unwrap();
        let (first_identity, second_identity) = {
            let ingress = super::lock(&host.inner.scan_ingress);
            let transfers = ingress.failure_transfers.values().next().unwrap();
            assert_eq!(transfers.len(), 2);
            let identity = |id| {
                transfers
                    .iter()
                    .find(|entry| entry.session.id == id)
                    .unwrap()
                    .occurrence
                    .as_ref()
                    .unwrap()
                    .identity()
            };
            (identity(first.id()), identity(second.id()))
        };
        assert_eq!(first_identity.host, second_identity.host);
        assert_eq!(first_identity.scan, second_identity.scan);
        assert_eq!(
            first_identity.source_generation,
            second_identity.source_generation
        );
        assert_eq!(first_identity.occurrence, second_identity.occurrence);
        ok(&call(
            &first,
            "scan.stop",
            &json!({"operationId":started["operationId"]}).to_string(),
        )
        .await);
        while let Some(signal) = host.inner.signals.pop() {
            let failure = matches!(signal, super::HostSignal::ScanFailedScoped(..));
            host.inner.handle(signal).await;
            if failure {
                break;
            }
        }
        assert_eq!(first_state.outbox.pending_scan_failure_owners(), 1);
        assert_eq!(second_state.outbox.pending_scan_failure_owners(), 1);
        let _ = host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saturated_scan_failures_keep_each_recipient_pending_until_terminal() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let first = host.open_session("first-failure").unwrap();
        let second = host.open_session("second-failure").unwrap();
        second.negotiate_scan_end_version("[1,2]").unwrap();
        super::lock(&host.inner.pump).take().unwrap().abort();
        let start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        for sequence in 0..super::SIGNALS_CAP as u64 {
            host.inner.signals.push(lifecycle(sequence));
        }
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailed {
                detail: "first failure".into()
            }),
            crate::IngressStatus::Accepted
        );
        ok(&call(
            &first,
            "scan.stop",
            &json!({"operationId":start["operationId"]}).to_string(),
        )
        .await);
        ok(&call(
            &second,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"second"}).to_string(),
        )
        .await);
        let _ = host.ingest(crate::RadioIngress::ScanFailed {
            detail: "second failure".into(),
        });
        assert_eq!(
            ok(&call(&second, "session.quiesce", "{}").await)["state"],
            "pending"
        );
        while let Some(signal) = host.inner.signals.pop() {
            if matches!(signal, super::HostSignal::ScanFailedScoped(..)) {
                host.inner.handle(signal).await;
            }
        }
        let records = parse(&second.drain(256, 65536))["records"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            of_type(&records, "scan-end")[0]["error"]["detail"],
            "second failure"
        );
        assert_eq!(
            ok(&call(&second, "session.quiesce", "{}").await)["state"],
            "sealed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deadline_after_core_start_keeps_widening_prefix_before_terminal() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "cancel-after-core-start".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        let first = host.open_session("first").unwrap();
        let second = host.open_session("second").unwrap();
        first.negotiate_scan_end_version("[1,2]").unwrap();
        ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":["180d"],"duplicatePolicy":"all","operationId":"first"})
                .to_string(),
        )
        .await);
        let _ = first.drain(256, 65536);
        gate.armed.store(true, Ordering::SeqCst);
        let gate_entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), gate_entered)
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *super::lock(&host.inner.after_scan_start) =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        let widening = tokio::spawn({
            let second = second.clone();
            async move {
                call(&second, "scan.start", &json!({"serviceUuids":["180f"],"duplicatePolicy":"all","operationId":"second","budgetMs":500}).to_string()).await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
                peer_id: "prefix".into(),
                service_uuids: vec![HR_SERVICE.into()],
                ..crate::Advertisement::default()
            })),
            crate::IngressStatus::Accepted
        );
        tokio::time::sleep(std::time::Duration::from_millis(550)).await;
        release.notify_one();
        assert_eq!(parse(&widening.await.unwrap())["ok"], false);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        let records = drain_until(&first, |records| !of_type(records, "scan-end").is_empty()).await;
        let terminal = records.iter().position(|r| r["t"] == "scan-end").unwrap();
        assert!(
            records[..terminal]
                .iter()
                .any(|r| r["t"] == "adv" && r["peerId"] == "prefix"),
            "{records:?}"
        );
        ok(&call(&first, "session.dispose", "{}").await);
        ok(&call(&second, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn saturated_scan_owner_pump_publishes_failures_before_shutdown() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "saturated-scan-pump".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        let first = host.open_session("first").unwrap();
        let second = host.open_session("second").unwrap();
        first.negotiate_scan_end_version("[1,2]").unwrap();
        second.negotiate_scan_end_version("[1,2]").unwrap();
        let first_start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        gate.armed.store(true, Ordering::SeqCst);
        let entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            host.inner.signals.push(lifecycle(sequence));
        }
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailed {
                detail: "first failure".into()
            }),
            crate::IngressStatus::Accepted
        );
        ok(&call(
            &first,
            "scan.stop",
            &json!({"operationId":first_start["operationId"]}).to_string(),
        )
        .await);
        ok(&call(
            &second,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"second"}).to_string(),
        )
        .await);
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailed {
                detail: "second failure".into()
            }),
            crate::IngressStatus::Accepted
        );
        assert_eq!(
            ok(&call(&first, "session.quiesce", "{}").await)["state"],
            "pending"
        );
        assert_eq!(
            ok(&call(&second, "session.quiesce", "{}").await)["state"],
            "pending"
        );
        ok(&call(&first, "session.dispose", "{}").await);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        let first_records =
            drain_until(&first, |records| !of_type(records, "scan-end").is_empty()).await;
        let second_records =
            drain_until(&second, |records| !of_type(records, "scan-end").is_empty()).await;
        assert_eq!(
            of_type(&first_records, "scan-end")[0]["error"]["detail"],
            "first failure"
        );
        assert_eq!(
            of_type(&second_records, "scan-end")[0]["error"]["detail"],
            "second failure"
        );
        let failure = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(failure) = host.ingress_failure()
                    && !failure["cleanup"].is_null()
                {
                    break failure;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(failure["error"]["operation"], "scan.source-ingress");
        assert_eq!(failure["cleanup"]["state"], "released");
        assert!(host.open_session("after-failure").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resumed_earlier_failure_precedes_already_popped_later_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let mut starts = 0;
        let radio = Scripted::new(Box::new(move |request| {
            if matches!(request, crate::RadioRequest::StartScan { .. }) {
                starts += 1;
                if starts == 2 {
                    tx.take().unwrap().send(request.id()).unwrap();
                    return Reply::Hold;
                }
            }
            polar_responder(request)
        }));
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let first = host.open_session("first").unwrap();
        let second = host.open_session("second").unwrap();
        first.negotiate_scan_end_version("[1,2]").unwrap();
        second.negotiate_scan_end_version("[1,2]").unwrap();
        super::lock(&host.inner.pump).take().unwrap().abort();
        let first_start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailed {
                detail: "earlier".into()
            }),
            crate::IngressStatus::Accepted
        );
        ok(&call(
            &first,
            "scan.stop",
            &json!({"operationId":first_start["operationId"]}).to_string(),
        )
        .await);
        let starting = tokio::spawn({
            let second = second.clone();
            async move {
                call(
                    &second,
                    "scan.start",
                    &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"second"})
                        .to_string(),
                )
                .await
            }
        });
        let generation = rx.await.unwrap();
        let earlier = loop {
            let signal = host.inner.signals.pop().expect("earlier failure queued");
            if matches!(signal, super::HostSignal::ScanFailedScoped(..)) {
                break signal;
            }
        };
        host.inner.handle(earlier).await;
        assert!(
            !super::lock(&host.inner.scan_ingress)
                .deferred_failures
                .is_empty()
        );
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailedScoped {
                scan_request_id: generation,
                detail: "later".into()
            }),
            crate::IngressStatus::Accepted
        );
        let later = loop {
            let signal = host.inner.signals.pop().expect("later failure queued");
            if matches!(signal, super::HostSignal::ScanFailedScoped(..)) {
                break signal;
            }
        };
        radio.answer(generation, crate::RadioCompletion::Unit);
        let _ = starting.await.unwrap();
        host.inner.handle(later).await;
        while let Some(signal) = host.inner.signals.pop() {
            if matches!(signal, super::HostSignal::ScanFailedScoped(..)) {
                host.inner.handle(signal).await;
            }
        }
        let earlier = parse(&first.drain(256, 65536));
        let later = parse(&second.drain(256, 65536));
        assert_eq!(
            of_type(earlier["records"].as_array().unwrap(), "scan-end")[0]["error"]["detail"],
            "earlier"
        );
        assert_eq!(
            of_type(later["records"].as_array().unwrap(), "scan-end")[0]["error"]["detail"],
            "later"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn held_start_failure_and_repeated_resets_do_not_stall_the_pump() {
        use crate::test_common::*;
        use serde_json::json;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let radio = Scripted::new(Box::new(move |request| {
            if matches!(request, crate::RadioRequest::StartScan { .. }) && tx.is_some() {
                tx.take().unwrap().send(request.id()).unwrap();
                Reply::Hold
            } else {
                polar_responder(request)
            }
        }));
        let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("held").unwrap();
        let independent = host.open_session("independent").unwrap();
        session.negotiate_scan_end_version("[1,2]").unwrap();
        let start = tokio::spawn({
            let session = session.clone();
            async move {
                call(
                    &session,
                    "scan.start",
                    &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start"})
                        .to_string(),
                )
                .await
            }
        });
        let generation = rx.await.unwrap();
        assert_eq!(
            host.ingest(crate::RadioIngress::AdvertisementScoped {
                scan_request_id: generation,
                advertisement: crate::Advertisement {
                    peer_id: "captured".into(),
                    ..crate::Advertisement::default()
                }
            }),
            crate::IngressStatus::Accepted
        );
        assert_eq!(
            host.ingest(crate::RadioIngress::ScanFailedScoped {
                scan_request_id: generation,
                detail: "failure before reset".into()
            }),
            crate::IngressStatus::Accepted
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while super::lock(&host.inner.scan_ingress)
                .deferred_failures
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let reset_scan = host.inner.central.active_scan_id().unwrap();
        for _ in 0..2 {
            host.inner.signals.push(super::HostSignal::AdapterReset(
                host.inner.central.attachment(),
                Some(reset_scan.clone()),
                ubm_desktop::AdapterLossCause::PoweredOff,
                Vec::new(),
            ));
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while super::lock(&host.inner.scan_ingress)
                    .deferred_reset
                    .is_none()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        host.inner.signals.push(super::HostSignal::ScanDeadlines);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !super::lock(&host.inner.scan_ingress).deferred_deadlines {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "unrelated".into(),
        });
        let progress = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            drain_until(&independent, |records| {
                !of_type(records, "ingress-drop").is_empty()
            }),
        )
        .await;
        assert_eq!(
            ok(&call(&session, "session.quiesce", "{}").await)["state"],
            "pending"
        );
        radio.answer(generation, crate::RadioCompletion::Unit);
        let _ = start.await.unwrap();
        assert!(progress.is_ok());
        let records =
            drain_until(&session, |records| !of_type(records, "scan-end").is_empty()).await;
        let terminal = records
            .iter()
            .position(|record| record["t"] == "scan-end")
            .unwrap();
        assert!(
            records[..terminal]
                .iter()
                .any(|record| record["t"] == "adv" && record["peerId"] == "captured")
        );
        assert_eq!(records[terminal]["error"]["detail"], "failure before reset");
        assert_eq!(of_type(&records, "scan-end").len(), 1);
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: None,
            failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "reset barrier"),
        });
        drain_until(&independent, |records| {
            !of_type(records, "security-failed").is_empty()
        })
        .await;
        let remaining = parse(&session.drain(256, 65536));
        assert!(
            of_type(remaining["records"].as_array().unwrap(), "scan-end").is_empty(),
            "repeated reset must not emit a second terminal"
        );
        ok(&call(&session, "session.dispose", "{}").await);
        ok(&call(&independent, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delayed_adapter_reset_does_not_end_replacement_scan() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "stale-reset-scan".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        let first = host.open_session("first").unwrap();
        let replacement = host.open_session("replacement").unwrap();
        let first_start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        let mut resets = host.inner.central.adapter_reset_events();
        let _ = first.drain(256, 65536);
        let _ = replacement.drain(256, 65536);
        gate.armed.store(true, Ordering::SeqCst);
        let entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap();
        let mut off = adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        let reset = tokio::time::timeout(std::time::Duration::from_secs(2), resets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reset.ended_scan.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !super::lock(&host.inner.signals.state)
                .queue
                .iter()
                .any(|signal| matches!(signal, super::HostSignal::AdapterReset(..)))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while host.inner.central.adapter_status().lost {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        ok(&call(
            &first,
            "scan.stop",
            &json!({"operationId":first_start["operationId"]}).to_string(),
        )
        .await);
        let replacement_start = ok(&call(
            &replacement,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"replacement"})
                .to_string(),
        )
        .await);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: None,
            failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "reset barrier"),
        });
        let records = drain_until(&replacement, |records| {
            !of_type(records, "security-failed").is_empty()
        })
        .await;
        assert!(
            of_type(&records, "scan-end").is_empty(),
            "old reset ended the replacement scan: {records:?}"
        );
        let first_records = parse(&first.drain(256, 65536));
        let terminals = of_type(first_records["records"].as_array().unwrap(), "scan-end");
        assert_eq!(
            terminals.len(),
            1,
            "reset terminal must survive a later stop: {first_records}"
        );
        assert_eq!(terminals[0]["operationId"], first_start["operationId"]);
        ok(&call(
            &replacement,
            "scan.stop",
            &json!({"operationId":replacement_start["operationId"]}).to_string(),
        )
        .await);
        ok(&call(&first, "session.dispose", "{}").await);
        ok(&call(&replacement, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_scan_cut_blocks_seal_and_survives_disposal() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "stale-reset-scan".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        let first = host.open_session("first").unwrap();
        let replacement = host.open_session("replacement").unwrap();
        let first_start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        let mut resets = host.inner.central.adapter_reset_events();
        let _ = first.drain(256, 65536);
        let _ = replacement.drain(256, 65536);
        gate.armed.store(true, Ordering::SeqCst);
        let entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap();
        assert_eq!(
            host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
                peer_id: "before-reset".into(),
                ..crate::Advertisement::default()
            })),
            crate::IngressStatus::Accepted
        );
        let mut off = adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        let reset = tokio::time::timeout(std::time::Duration::from_secs(2), resets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reset.ended_scan.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !super::lock(&host.inner.signals.state)
                .queue
                .iter()
                .any(|signal| matches!(signal, super::HostSignal::AdapterReset(..)))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            ok(&call(&first, "session.quiesce", "{}").await)["state"],
            "pending",
            "captured reset must fence sealing"
        );
        ok(&call(&first, "session.dispose", "{}").await);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: None,
            failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "reset barrier"),
        });
        drain_until(&replacement, |records| {
            !of_type(records, "security-failed").is_empty()
        })
        .await;
        let first_records = parse(&first.drain(256, 65536));
        let records = first_records["records"].as_array().unwrap();
        let advertisement = records
            .iter()
            .position(|record| record["t"] == "adv" && record["peerId"] == "before-reset")
            .expect("captured advertisement");
        let terminal = records
            .iter()
            .position(|record| {
                record["t"] == "scan-end" && record["operationId"] == first_start["operationId"]
            })
            .expect("captured terminal");
        assert!(
            advertisement < terminal,
            "accepted prefix must precede reset terminal: {first_records}"
        );
        assert_eq!(of_type(records, "scan-end").len(), 1);
        ok(&call(&replacement, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_terminal_alone_blocks_sealing() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "stale-reset-scan".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        let first = host.open_session("first").unwrap();
        let replacement = host.open_session("replacement").unwrap();
        let first_start = ok(&call(
            &first,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
        )
        .await);
        let mut resets = host.inner.central.adapter_reset_events();
        let _ = first.drain(256, 65536);
        let _ = replacement.drain(256, 65536);
        gate.armed.store(true, Ordering::SeqCst);
        let entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap();
        let mut off = adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        let reset = tokio::time::timeout(std::time::Duration::from_secs(2), resets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reset.ended_scan.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !super::lock(&host.inner.signals.state)
                .queue
                .iter()
                .any(|signal| matches!(signal, super::HostSignal::AdapterReset(..)))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            ok(&call(&first, "session.quiesce", "{}").await)["state"],
            "pending",
            "captured reset must fence sealing"
        );
        ok(&call(&first, "session.dispose", "{}").await);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: None,
            failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "reset barrier"),
        });
        drain_until(&replacement, |records| {
            !of_type(records, "security-failed").is_empty()
        })
        .await;
        let first_records = parse(&first.drain(256, 65536));
        let records = first_records["records"].as_array().unwrap();
        assert!(
            records.iter().any(|record| record["t"] == "scan-end"
                && record["operationId"] == first_start["operationId"]),
            "captured terminal: {first_records}"
        );
        assert_eq!(of_type(records, "scan-end").len(), 1);
        ok(&call(&replacement, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delayed_adapter_reset_does_not_retire_new_connection_or_subscription() {
        use crate::test_common::*;
        use serde_json::json;
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        struct Gate {
            armed: AtomicBool,
            entered: tokio::sync::Notify,
            release: (Mutex<bool>, Condvar),
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.release.0.lock().unwrap() = true;
                self.0.release.1.notify_all();
            }
        }
        impl super::WakeSink for Gate {
            fn wake(&self, _: u64) {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    let mut released = self.release.0.lock().unwrap();
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
            }
        }
        let radio = Scripted::polar();
        let gate = Arc::new(Gate {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        });
        let _release = Release(Arc::clone(&gate));
        let host = Arc::new(
            super::MobileHost::open(
                Arc::clone(&radio) as Arc<dyn crate::PlatformRadio>,
                Arc::clone(&gate) as Arc<dyn super::WakeSink>,
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "stale-reset-scan".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        let first = host.open_session("first").unwrap();
        let replacement = host.open_session("replacement").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.to_owned(),
            address: Some(POLAR.to_owned()),
            ..crate::Advertisement::default()
        }));
        ok(&call(
            &first,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"old-connect"}).to_string(),
        )
        .await);
        let mut resets = host.inner.central.adapter_reset_events();
        let _ = first.drain(256, 65536);
        let _ = replacement.drain(256, 65536);
        gate.armed.store(true, Ordering::SeqCst);
        let entered = gate.entered.notified();
        host.ingest(crate::RadioIngress::Dropped {
            class: crate::IngressClass::Control,
            detail: "hold pump".into(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap();
        let mut off = adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        let reset = tokio::time::timeout(std::time::Duration::from_secs(2), resets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reset.ended_scan.is_none());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !super::lock(&host.inner.signals.state)
                .queue
                .iter()
                .any(|signal| matches!(signal, super::HostSignal::AdapterReset(..)))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        host.ingest(crate::RadioIngress::AdapterState(adapter_on()));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while host.inner.central.adapter_status().lost {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        ok(&call(
            &replacement,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new-connect"}).to_string(),
        )
        .await);
        ok(&call(
            &replacement,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new-discover"}).to_string(),
        )
        .await);
        ok(&call(&replacement, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"new-hr","deliveryMode":"require-notification","operationId":"new-sub"}).to_string()).await);
        *gate.release.0.lock().unwrap() = true;
        gate.release.1.notify_all();
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: None,
            failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "reset barrier"),
        });
        drain_until(&replacement, |records| {
            !of_type(records, "security-failed").is_empty()
        })
        .await;
        let epoch = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|request| match request {
                crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .expect("new notification epoch");
        assert_eq!(
            host.ingest(crate::RadioIngress::Notification {
                instance: crate::Instance {
                    peer_id: POLAR.into(),
                    service_uuid: HR_SERVICE.into(),
                    service_occurrence: 0,
                    characteristic_uuid: HR_MEASUREMENT.into(),
                    characteristic_occurrence: 0
                },
                epoch,
                value: vec![42],
            }),
            crate::IngressStatus::Accepted
        );
        let values = drain_until(&replacement, |records| {
            !of_type(records, "value").is_empty()
        })
        .await;
        assert_eq!(of_type(&values, "value")[0]["consumer"], "new-hr");
        let unsubscribed = ok(&call(&replacement, "gatt.unsubscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"new-hr","operationId":"new-unsub"}).to_string()).await);
        assert_eq!(
            unsubscribed["physicalDisabled"], true,
            "old reset retired the new subscription: {unsubscribed}"
        );
        let disconnects_before = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| matches!(request, crate::RadioRequest::Disconnect { .. }))
            .count();
        let disconnected = ok(&call(
            &replacement,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new-disconnect"}).to_string(),
        )
        .await);
        assert_eq!(disconnected["state"], "released");
        let disconnects_after = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| matches!(request, crate::RadioRequest::Disconnect { .. }))
            .count();
        assert_eq!(
            disconnects_after,
            disconnects_before + 1,
            "new lease must release its physical connection"
        );
        ok(&call(&first, "session.dispose", "{}").await);
        ok(&call(&replacement, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }

    #[test]
    fn scan_deadline_marker_is_reserved_and_coalesced_under_control_pressure() {
        let signals = super::Signals::default();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            signals.push(lifecycle(sequence));
        }
        for _ in 0..1000 {
            signals.push(super::HostSignal::ScanDeadlines);
        }
        assert_eq!(signals.queue_len(), super::SIGNALS_CAP + 1);
        let mut deadlines = 0;
        while let Some(signal) = signals.pop() {
            if matches!(signal, super::HostSignal::ScanDeadlines) {
                deadlines += 1;
            }
        }
        assert_eq!(deadlines, 1);
        assert_eq!(signals.take_overflow(), (0, [0, 0, 0]));
        signals.push(super::HostSignal::ScanDeadlines);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::ScanDeadlines)
        ));
    }

    #[test]
    fn popped_failures_remain_charged_to_admission_budget() {
        let signals = super::Signals::default();
        let failure = |detail| {
            ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::ScanStartFailed,
                ubm_core::contracts::BleErrorDomain::Scan,
                "scan.start",
            )
            .with_detail(detail)
        };
        for generation in 0..super::SIGNALS_CAP as u64 {
            assert_eq!(
                signals.push_scan_failure(
                    failure("held"),
                    generation,
                    Vec::new(),
                    Arc::new(()),
                    "radio"
                ),
                super::ScanFailureAdmission::Queued
            );
            assert!(matches!(
                signals.pop(),
                Some(super::HostSignal::ScanFailedScoped(..))
            ));
        }
        assert_eq!(
            signals.push_scan_failure(failure("reserved"), 2000, Vec::new(), Arc::new(()), "radio"),
            super::ScanFailureAdmission::Queued
        );
        assert_eq!(
            signals.push_scan_failure(
                failure("emergency"),
                2001,
                Vec::new(),
                Arc::new(()),
                "radio"
            ),
            super::ScanFailureAdmission::Emergency
        );
        assert_eq!(
            signals.push_scan_failure(failure("excess"), 2002, Vec::new(), Arc::new(()), "radio"),
            super::ScanFailureAdmission::Closed
        );
    }

    #[tokio::test]
    async fn coalesced_empty_reset_snapshots_do_not_accumulate_sessions() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, crate::MobilePlatform::Android).await;
        let _session = host.open_session("empty-reset-snapshot").unwrap();
        let signals = super::Signals::default();
        let attachment = host.inner.central.attachment();
        for _ in 0..(super::SIGNALS_CAP * 2) {
            let resources = host
                .inner
                .session_list()
                .into_iter()
                .map(|session| {
                    let captured = session.capture_reset_resources(&attachment, &[], &[]);
                    (session, captured)
                })
                .collect();
            signals.push(super::HostSignal::AdapterReset(
                attachment.clone(),
                None,
                ubm_desktop::AdapterLossCause::PoweredOff,
                resources,
            ));
        }
        let Some(super::HostSignal::AdapterReset(_, _, _, resources)) = signals.pop() else {
            panic!("reset marker missing")
        };
        assert!(
            resources.is_empty(),
            "empty snapshots must not retain sessions: {}",
            resources.len()
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn adapter_reset_retry_keeps_its_marker_under_control_pressure() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, crate::MobilePlatform::Android).await;
        let signals = super::Signals::default();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            signals.push(lifecycle(sequence));
        }
        let attachment = host.inner.central.attachment();
        for _ in 0..2 {
            signals.push(super::HostSignal::AdapterReset(
                attachment.clone(),
                Some(super::OperationId::new("scan-reset").unwrap()),
                ubm_desktop::AdapterLossCause::PoweredOff,
                Vec::new(),
            ));
        }
        assert_eq!(signals.queue_len(), super::SIGNALS_CAP + 1);
        let mut resets = 0;
        while let Some(signal) = signals.pop() {
            if matches!(signal, super::HostSignal::AdapterReset(..)) {
                resets += 1;
            }
        }
        assert_eq!(resets, 1);
        assert_eq!(signals.take_overflow(), (0, [0, 0, 0]));
        host.shutdown().await;
    }

    #[tokio::test]
    async fn advertisement_batch_yields_to_deadline_without_losing_remaining_records() {
        let signals = super::Signals::default();
        signals.push(super::HostSignal::Advertisements);
        signals.push(super::HostSignal::ScanDeadlines);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::Advertisements)
        ));
        let mut source = 0..(super::ADVERTISEMENT_BATCH * 2 + 1);
        let mut delivered = Vec::new();
        let filled = super::pump_advertisement_batch(
            || std::future::ready(source.next()),
            |value| delivered.push(value),
        )
        .await;
        assert_eq!(delivered.len(), super::ADVERTISEMENT_BATCH);
        assert!(filled);
        signals.push(super::HostSignal::Advertisements);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::ScanDeadlines)
        ));
        while let Some(signal) = signals.pop() {
            assert!(matches!(signal, super::HostSignal::Advertisements));
            if super::pump_advertisement_batch(
                || std::future::ready(source.next()),
                |value| delivered.push(value),
            )
            .await
            {
                // A producer can concurrently queue the same marker. The
                // continuation must coalesce, not create duplicate turns.
                signals.push(super::HostSignal::Advertisements);
                signals.push(super::HostSignal::Advertisements);
                assert_eq!(signals.queue_len(), 1);
            }
        }
        assert_eq!(
            delivered,
            (0..(super::ADVERTISEMENT_BATCH * 2 + 1)).collect::<Vec<_>>()
        );
        assert_eq!(signals.take_overflow(), (0, [0, 0, 0]));
    }

    #[test]
    fn half_open_shutdown_receipt_preserves_native_cause_and_unrelated_failures() {
        let error = ubm_desktop::DesktopError::new(
            ubm_core::contracts::BleErrorCode::PlatformFailure,
            ubm_core::contracts::BleErrorDomain::Connection,
            "radio.disconnect.compensation",
        )
        .with_platform(
            ubm_desktop::PlatformDetail::new("android", "133").with_message("disconnect refused"),
        );
        let report = ubm_desktop::ShutdownReport {
            record: Ok(ubm_core::ownership::CleanupRecord::new(
                None,
                ubm_core::ownership::CleanupState::Released,
                vec![],
            )
            .unwrap()),
            radio_close_failures: vec![],
            transport_close_failures: vec![],
            peer_failure_recovery_debts: vec![],
            half_open_close_failures: vec![error.clone()],
            destroy_steps: 1,
            scan_stop_failure: None,
        };
        let failures = super::shutdown_failures(report);
        assert_eq!(
            failures,
            vec![crate::session::cleanup_failure("connection", &error)]
        );
        let unrelated = ubm_core::ownership::CleanupFailure::new(
            "subscription".into(),
            ubm_core::contracts::BleErrorCode::GattSubscribeFailed,
        )
        .unwrap();
        let retry = ubm_desktop::ShutdownReport {
            record: Ok(ubm_core::ownership::CleanupRecord::new(
                None,
                ubm_core::ownership::CleanupState::ReleaseFailed,
                vec![unrelated],
            )
            .unwrap()),
            radio_close_failures: vec![],
            transport_close_failures: vec![],
            peer_failure_recovery_debts: vec![],
            half_open_close_failures: vec![],
            destroy_steps: 1,
            scan_stop_failure: None,
        };
        let failures = super::shutdown_failures(retry);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["resourceKind"], "subscription");
        assert_eq!(failures[0]["code"], "gatt.subscribe-failed");
    }
    #[test]
    fn stale_lifecycle_generation_cannot_reset_new_security_owner() {
        let mut entry = super::SecurityEntry {
            state: crate::radio::SecurityState {
                bond: crate::radio::BondState::Bonded,
                encryption: crate::radio::EncryptionState::Encrypted,
                authentication: crate::radio::AuthenticationState::Authenticated,
                secure_connections: crate::radio::SecureConnectionsState::Yes,
                pairing_possible: Some(true),
            },
            owner_generation: Some("generation-b".into()),
        };
        assert!(super::reset_security_for_generation(&mut entry, "generation-a").is_none());
        assert_eq!(
            entry.state.encryption,
            crate::radio::EncryptionState::Encrypted
        );
        assert_eq!(
            entry.state.authentication,
            crate::radio::AuthenticationState::Authenticated
        );
        assert_eq!(
            entry.state.secure_connections,
            crate::radio::SecureConnectionsState::Yes
        );
        assert_eq!(entry.state.bond, crate::radio::BondState::Bonded);
        let reset = super::reset_security_for_generation(&mut entry, "generation-b")
            .expect("current owner lifecycle resets security");
        assert_eq!(reset.encryption, crate::radio::EncryptionState::Unknown);
        assert_eq!(
            reset.authentication,
            crate::radio::AuthenticationState::Unknown
        );
        assert_eq!(
            reset.secure_connections,
            crate::radio::SecureConnectionsState::Unknown
        );
        assert_eq!(reset.bond, crate::radio::BondState::Bonded);
        assert!(entry.owner_generation.is_none());
    }

    #[test]
    fn same_generation_join_does_not_take_or_clear_security_ownership() {
        let mut entry = super::SecurityEntry {
            state: crate::radio::SecurityState {
                bond: crate::radio::BondState::Bonded,
                encryption: crate::radio::EncryptionState::Encrypted,
                authentication: crate::radio::AuthenticationState::Authenticated,
                secure_connections: crate::radio::SecureConnectionsState::Yes,
                pairing_possible: Some(true),
            },
            owner_generation: Some("generation-a".into()),
        };
        assert!(super::reset_security_for_generation(&mut entry, "generation-b").is_none());
        assert_eq!(entry.owner_generation.as_deref(), Some("generation-a"));
        assert_eq!(
            entry.state.encryption,
            crate::radio::EncryptionState::Encrypted
        );
        assert!(super::reset_security_for_generation(&mut entry, "generation-a").is_some());
    }

    #[derive(Default)]
    struct HoldConnect {
        cancels: std::sync::Mutex<Vec<crate::RequestId>>,
        requests: std::sync::Mutex<Vec<crate::RequestId>>,
    }

    impl super::PlatformRadio for HoldConnect {
        fn submit(&self, request: crate::RadioRequest) {
            self.requests.lock().unwrap().push(request.id());
        }

        fn cancel(&self, request_id: crate::RequestId) {
            self.cancels.lock().unwrap().push(request_id);
        }
    }

    #[tokio::test]
    async fn dropped_connect_releases_only_its_generation_admission() {
        let radio = std::sync::Arc::new(HoldConnect::default());
        let host = super::MobileHost::open(
            radio.clone(),
            std::sync::Arc::new(NoWake),
            super::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "test".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap();

        let mut first = Box::pin(ubm_desktop::RadioBoundary::connect(
            &host.inner.radio,
            "peer",
            "generation-a",
        ));
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            super::lock(&host.inner.security)
                .get("peer")
                .and_then(|entry| entry.owner_generation.as_deref()),
            Some("generation-a")
        );
        let issued_id = radio.requests.lock().unwrap()[0];
        drop(first);
        assert_eq!(radio.cancels.lock().unwrap().as_slice(), &[issued_id]);
        assert!(
            super::lock(&host.inner.security)
                .get("peer")
                .and_then(|entry| entry.owner_generation.as_deref())
                .is_none(),
            "dropping the connect future must compensate its reservation"
        );

        let mut abandoned_a = Box::pin(ubm_desktop::RadioBoundary::connect(
            &host.inner.radio,
            "peer",
            "generation-a",
        ));
        std::future::poll_fn(|cx| {
            assert!(abandoned_a.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        host.inner.stamp_security_generation("peer", "generation-b");
        drop(abandoned_a);
        assert_eq!(
            super::lock(&host.inner.security)
                .get("peer")
                .and_then(|entry| entry.owner_generation.as_deref()),
            Some("generation-b"),
            "an abandoned A must not clear a newer B owner"
        );
        host.ingest(crate::RadioIngress::SecurityChanged {
            peer_id: "peer".into(),
            state: crate::radio::SecurityState {
                bond: crate::BondState::Bonded,
                encryption: crate::EncryptionState::Encrypted,
                authentication: crate::AuthenticationState::Authenticated,
                secure_connections: crate::SecureConnectionsState::Yes,
                pairing_possible: Some(true),
            },
        });
        assert_eq!(
            super::lock(&host.inner.security)
                .get("peer")
                .and_then(|entry| entry.owner_generation.as_deref()),
            Some("generation-b"),
            "unscoped security observations do not overwrite the owned snapshot"
        );
        host.ingest(crate::RadioIngress::SecurityFailed {
            peer_id: Some("peer".into()),
            failure: crate::PlatformFailure::new(
                crate::radio::FailureKind::Platform,
                "security observation failed",
            ),
        });
        assert_eq!(
            super::lock(&host.inner.security)
                .get("peer")
                .and_then(|entry| entry.owner_generation.as_deref()),
            Some("generation-b"),
            "unscoped failures do not clear the owned snapshot"
        );
        assert_eq!(
            host.inner
                .set_security_state_if_owner(
                    "peer",
                    Some("generation-a"),
                    crate::radio::SecurityState {
                        bond: crate::BondState::Bonded,
                        encryption: crate::EncryptionState::Encrypted,
                        authentication: crate::AuthenticationState::Authenticated,
                        secure_connections: crate::SecureConnectionsState::Yes,
                        pairing_possible: Some(true),
                    },
                    "security.state",
                )
                .unwrap_err()
                .code(),
            ubm_core::contracts::BleErrorCode::ConnectionStale
        );

        super::lock(&host.inner.security)
            .get_mut("peer")
            .expect("security owner")
            .state = crate::radio::SecurityState {
            bond: crate::BondState::Bonded,
            encryption: crate::EncryptionState::Encrypted,
            authentication: crate::AuthenticationState::Authenticated,
            secure_connections: crate::SecureConnectionsState::Yes,
            pairing_possible: Some(true),
        };
        let mut joined = Box::pin(ubm_desktop::RadioBoundary::connect(
            &host.inner.radio,
            "peer",
            "generation-b",
        ));
        std::future::poll_fn(|cx| {
            assert!(joined.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(joined);
        let entry = super::lock(&host.inner.security)
            .get("peer")
            .cloned()
            .expect("security owner");
        assert_eq!(entry.owner_generation.as_deref(), Some("generation-b"));
        assert_eq!(entry.state.encryption, crate::EncryptionState::Encrypted);
        assert_eq!(
            entry.state.authentication,
            crate::AuthenticationState::Authenticated
        );
        assert_eq!(
            entry.state.secure_connections,
            crate::SecureConnectionsState::Yes
        );
        host.shutdown().await;
    }

    struct NoRadio;
    impl super::PlatformRadio for NoRadio {
        fn submit(&self, _: crate::RadioRequest) {
            panic!("admission test must not use radio");
        }
        fn cancel(&self, _: crate::RequestId) {}
    }
    struct NoWake;
    impl super::WakeSink for NoWake {
        fn wake(&self, _: u64) {}
    }
    #[tokio::test]
    async fn route_poll_failure_keeps_its_structured_error() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, crate::MobilePlatform::Android).await;
        let session = host.open_session("poll-test").unwrap();
        let route = super::Route {
            session_id: session.id(),
            consumer: "consumer".into(),
            core_consumer: "core-consumer".into(),
            peer_id: "unknown-peer".into(),
            selector: ubm_core::central::PathSelector {
                service_uuid: "180d".into(),
                service_occurrence: Some(0),
                characteristic_uuid: Some("2a37".into()),
                characteristic_occurrence: Some(0),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            },
            delivery: "notification",
            recorded: None,
            terminal_evidence_scope: None,
            terminal: None,
        };
        let expected = host
            .inner
            .central
            .poll_notification(&route.peer_id, &route.selector, &route.core_consumer)
            .await
            .unwrap_err();
        let super::RouteDrain::Ended {
            terminal,
            loss_accounted,
            ..
        } = host.inner.drain_route(&route, 1).await
        else {
            panic!("poll failure must end route")
        };
        assert_eq!(terminal.0, "source-failed");
        assert_eq!(terminal.3.as_ref(), Some(&expected));
        host.inner.end_route(&route, terminal, loss_accounted);
        let batch: serde_json::Value = serde_json::from_str(&session.drain(16, 65536)).unwrap();
        let record = batch["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["t"] == "stream-end")
            .unwrap();
        assert_eq!(record["reason"], "source-failed");
        assert_eq!(
            record["error"],
            serde_json::Value::Object(crate::wire::error_object(&expected))
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn removed_session_terminal_is_committed_for_explicit_recovery() {
        let Some(directory) = crate::test_common::recording_test_directory(
            "host::signal_tests::removed_session_terminal_is_committed_for_explicit_recovery",
        ) else {
            return;
        };
        let _ = crate::continuation::configure_terminal_evidence_maximum_bytes(65_536);
        let registry =
            std::sync::Arc::new(ubm_desktop::continuation_journal::JournalRegistry::default());
        registry
            .configure_directory(&directory)
            .expect("configure private registry directory");
        let radio = crate::test_common::Scripted::polar();
        let wake = std::sync::Arc::new(crate::test_common::Wakes::default());
        let host = std::sync::Arc::new(
            super::MobileHost::open_with_recording_registry(
                std::sync::Arc::clone(&radio) as std::sync::Arc<dyn super::PlatformRadio>,
                std::sync::Arc::clone(&wake) as std::sync::Arc<dyn super::WakeSink>,
                super::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "terminal-race".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
                registry,
            )
            .await
            .expect("open durable host"),
        );
        radio.bind_host(&host);
        let session = host.open_session("terminal-race").expect("session");
        let scope = super::ScanTerminalEvidenceScope {
            manager_id: "terminal-race".into(),
            operation_id: "subscription-terminal".into(),
            incarnation: "1".into(),
            scope_utf16le: None,
        };
        let ledger = host
            .inner
            .terminal_evidence_ledger()
            .expect("durable terminal ledger");
        ledger
            .reserve(
                &ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
                    producer: "subscription".into(),
                    manager_id: scope.manager_id.clone(),
                    operation_id: scope.operation_id.clone(),
                    incarnation: scope.incarnation.clone(),
                    scope_utf16le: None,
                },
                65_536,
            )
            .expect("reserve subscription terminal");
        let route = super::Route {
            session_id: session.id(),
            consumer: "consumer".into(),
            core_consumer: "core-consumer".into(),
            peer_id: "peer".into(),
            selector: ubm_core::central::PathSelector {
                service_uuid: "180d".into(),
                service_occurrence: Some(0),
                characteristic_uuid: Some("2a37".into()),
                characteristic_occurrence: Some(0),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            },
            delivery: "notification",
            recorded: None,
            terminal_evidence_scope: Some(scope),
            terminal: None,
        };
        let weak_host = Arc::downgrade(&host);
        let removed_session_id = session.id();
        *super::lock(&host.inner.subscription_terminal_evidence_after_commit) =
            Some(Arc::new(move || {
                weak_host
                    .upgrade()
                    .expect("host remains alive during terminal publication")
                    .inner
                    .remove_session(removed_session_id);
            }));
        host.inner
            .end_route(&route, ("source-failed", 0, 0, None), false);
        let recovery = host
            .terminal_evidence_recovery_selected()
            .expect("recovery controller");
        let page = recovery.list(8, 65_536).expect("list terminal evidence");
        assert_eq!(
            page.entries.len(),
            1,
            "removed session cannot discard durable terminal evidence"
        );
        assert_eq!(page.entries[0].producer, "subscription");
        assert_eq!(
            page.entries[0].state,
            crate::TerminalEvidenceRecoveryState::Committed
        );
        recovery
            .acknowledge(&page.entries[0])
            .expect("explicit recovery acknowledgement");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&host.shutdown().await)
                .expect("shutdown record")["state"],
            "released"
        );
    }

    #[tokio::test]
    async fn recovery_close_waits_for_an_admitted_list_and_leaves_no_lease_lock() {
        let Some(directory) = crate::test_common::recording_test_directory(
            "host::signal_tests::recovery_close_waits_for_an_admitted_list_and_leaves_no_lease_lock",
        ) else {
            return;
        };
        let _ = crate::continuation::configure_terminal_evidence_maximum_bytes(65_536);
        let registry = Arc::new(ubm_desktop::continuation_journal::JournalRegistry::default());
        registry.configure_directory(&directory).unwrap();
        let radio = crate::test_common::Scripted::polar();
        let wake = Arc::new(crate::test_common::Wakes::default());
        let host = Arc::new(
            super::MobileHost::open_with_recording_registry(
                Arc::clone(&radio) as Arc<dyn super::PlatformRadio>,
                Arc::clone(&wake) as Arc<dyn super::WakeSink>,
                super::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: "recovery-close-list".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
                registry,
            )
            .await
            .unwrap(),
        );
        radio.bind_host(&host);
        host.seed_terminal_evidence_for_test("scan", "manager", "operation", "1", b"row", false)
            .unwrap();
        let recovery = host.terminal_evidence_recovery_selected().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        *super::lock(&recovery.lease.before_list_claim) = Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            lock(&release_rx).recv().unwrap();
        }));
        let listing = recovery.clone();
        let listed = std::thread::spawn(move || listing.list(8, 65_536));
        entered_rx.recv().unwrap();
        let closing = recovery.clone();
        let closed = std::thread::spawn(move || closing.close());
        release_tx.send(()).unwrap();
        assert_eq!(listed.join().unwrap().unwrap().entries.len(), 1);
        closed.join().unwrap().unwrap();
        assert!(recovery.list(8, 65_536).is_err());
        let later = host.terminal_evidence_recovery_selected().unwrap();
        assert_eq!(later.list(8, 65_536).unwrap().entries.len(), 1);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn accepted_open_cannot_publish_after_host_closes_admission() {
        let host = super::MobileHost::open(
            std::sync::Arc::new(NoRadio),
            std::sync::Arc::new(NoWake),
            super::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "test".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered_tx));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let waiting = release.clone();
        *super::lock(&host.inner.before_session_admission) = Some(std::sync::Arc::new(move || {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            waiting.wait();
        }));
        let opening = host.clone();
        let result = tokio::task::spawn_blocking(move || opening.open_session("racing"));
        entered_rx.await.unwrap();
        host.inner
            .shut_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        release.wait();
        assert!(
            result.await.unwrap().is_err(),
            "accepted open published after source failure closed the owner"
        );
        assert!(super::lock(&host.inner.sessions).is_empty());
    }
    use super::*;
    use crate::radio::{AuthenticationState, EncryptionState, SecureConnectionsState};

    fn lifecycle(sequence: u64) -> HostSignal {
        HostSignal::Lifecycle(LifecycleEvent {
            sequence,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
            peer_key: "key".to_owned(),
            connection_generation: None,
            database_generation: None,
            kind: LifecycleKind::LinkLost,
            platform: None,
        })
    }

    fn security(peer: &str, bonded: BondState) -> HostSignal {
        HostSignal::Security(
            peer.to_owned(),
            SecurityState {
                bond: bonded,
                encryption: EncryptionState::Unknown,
                authentication: AuthenticationState::Unknown,
                secure_connections: SecureConnectionsState::Unknown,
                pairing_possible: None,
            },
        )
    }

    fn scope(peer: &str) -> InstanceKey {
        (peer.to_owned(), "180d".to_owned(), 0, "2a37".to_owned(), 0)
    }

    fn refused(
        failure: ubm_desktop::continuation_outbox::DataIngressFailure,
        items: u64,
        bytes: u64,
    ) -> ubm_desktop::continuation_outbox::DataBatchRejection {
        ubm_desktop::continuation_outbox::DataBatchRejection {
            failure,
            items,
            bytes,
        }
    }

    /// Every value a refused group polled lands in its route terminal exactly
    /// once, under the frozen lifecycle names, with the core's own loss.
    #[test]
    fn refused_group_terminals_count_each_polled_value_once() {
        use ubm_desktop::continuation_outbox::DataIngressFailure as Failure;
        let storage = Failure::Storage {
            bytes: 10,
            error: ubm_desktop::continuation_journal::JournalError {
                kind: "storage.full",
                detail: "capacity",
                operation: "append",
                sqlite_extended_code: None,
                sqlite_code: None,
            },
        };
        assert_eq!(
            refusal_terminal(&refused(storage, 5, 70), None),
            Some(("closed", 5, 70, None))
        );
        assert_eq!(
            refusal_terminal(
                &refused(Failure::Overflow { bytes: 10 }, 3, 30),
                Some(("overflow", 4, 400, None))
            ),
            Some(("overflow", 7, 430, None))
        );
        // Already sealed: the cutoff counted the group and must not fabricate
        // an overflow terminal, exactly like a stopped journal.
        assert_eq!(
            refusal_terminal(&refused(Failure::Sealed { bytes: 10 }, 3, 30), None),
            None
        );
        // A stopped journal: the first value is the handoff cutoff itself.
        let stopped = Failure::Stopped { bytes: 10 };
        assert_eq!(
            refusal_terminal(&refused(stopped.clone(), 1, 10), None),
            None
        );
        // A stopped journal has already counted every refused value after its
        // cutoff. A multi-record batch must not manufacture an overflow
        // terminal for the later records.
        assert_eq!(
            refusal_terminal(&refused(stopped.clone(), 4, 45), None),
            None
        );
        assert_eq!(
            refusal_terminal(&refused(stopped, 2, 25), Some(("overflow", 1, 8, None))),
            Some(("overflow", 1, 8, None))
        );
    }

    #[test]
    fn stopped_batch_accounting_uses_the_real_outbox_cutoff_across_bounded_groups() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-mobile-stopped-batch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = ubm_desktop::continuation_journal::JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1000,
        };
        let journal = std::sync::Arc::new(
            ubm_desktop::continuation_journal::ContinuationJournal::open(
                &path,
                "stopped-batch",
                &serde_json::json!({"onAppearance":"native"}),
                quota,
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .attach_journal(journal.clone(), serde_json::json!({"epoch":"a"}))
            .unwrap();
        outbox
            .register_journal_consumer("c", serde_json::json!({"generation":"a"}))
            .unwrap();
        ubm_desktop::continuation_journal::ContinuationJournal::open(
            &path,
            "stopped-batch",
            &serde_json::json!({"onAppearance":"native"}),
            quota,
        )
        .unwrap()
        .stop()
        .unwrap();
        let records: Vec<Value> = (0..70)
            .map(|index| serde_json::json!({"t":"value", "consumer":"c", "n":index}))
            .collect();
        let expected_bytes = records
            .iter()
            .map(|record| record.to_string().len() as u64)
            .sum::<u64>();
        let outcome = outbox.push_data_batch(records);
        assert_eq!(outcome.accepted, 0);
        let rejected = outcome
            .rejected
            .expect("the stopped journal rejects the batch");
        assert!(matches!(
            rejected.failure,
            ubm_desktop::continuation_outbox::DataIngressFailure::Stopped { .. }
        ));
        assert_eq!((rejected.items, rejected.bytes), (70, expected_bytes));
        assert_eq!(
            outbox.after_cutoff_loss(),
            ubm_desktop::continuation_outbox::AfterCutoffLoss {
                items: 70,
                bytes: expected_bytes,
            }
        );

        assert_eq!(refusal_terminal(&rejected, None), None);
        assert_eq!(
            refusal_terminal(&rejected, Some(("overflow", 1, 8, None))),
            Some(("overflow", 1, 8, None))
        );
        let late_records: Vec<Value> = (70..73)
            .map(|index| serde_json::json!({"t":"value", "consumer":"c", "n":index}))
            .collect();
        let late_bytes = late_records
            .iter()
            .map(|record| record.to_string().len() as u64)
            .sum::<u64>();
        let late = outbox.push_data_batch(late_records);
        assert_eq!(late.accepted, 0);
        let late_rejected = late
            .rejected
            .expect("a post-cutoff batch is sealed and rejected");
        assert!(matches!(
            late_rejected.failure,
            ubm_desktop::continuation_outbox::DataIngressFailure::Sealed { .. }
        ));
        assert_eq!((late_rejected.items, late_rejected.bytes), (3, late_bytes));
        assert_eq!(refusal_terminal(&late_rejected, None), None);
        assert_eq!(
            outbox.after_cutoff_loss(),
            ubm_desktop::continuation_outbox::AfterCutoffLoss {
                items: 73,
                bytes: expected_bytes + late_bytes,
            }
        );
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// X-R6: a stalled pump (no pops) plus a burst stays bounded, and every
    /// refused signal is counted exactly once.
    #[test]
    fn signal_burst_is_bounded() {
        let signals = Signals::default();
        for sequence in 0..5000 {
            signals.push(lifecycle(sequence));
        }
        let mut drained = 0u64;
        while signals.pop().is_some() {
            drained += 1;
        }
        let (lost, drops) = signals.take_overflow();
        assert!(
            drained <= SIGNALS_CAP as u64,
            "bounded retention, got {drained}"
        );
        assert_eq!(drained + lost, 5000, "exact accounting");
        assert_eq!(drops, [0, 0, 0]);
    }

    /// Current-state facts merge per scope: the latest wins, queued once.
    #[test]
    fn countable_classes_coalesce_per_scope() {
        let signals = Signals::default();
        signals.push(security("peer-a", BondState::NotBonded));
        signals.push(security("peer-a", BondState::Bonded));
        signals.push(security("peer-b", BondState::Bonded));
        signals.push(HostSignal::ScanFailed("one".to_owned()));
        signals.push(HostSignal::ScanFailed("two".to_owned()));
        signals.push(HostSignal::IngressDrop(IngressClass::Control, 2));
        signals.push(HostSignal::IngressDrop(IngressClass::Control, 3));
        signals.push(HostSignal::IngressDrop(IngressClass::Advertisement, 1));
        signals.push_value(scope("peer-a"));
        signals.push_value(scope("peer-a"));
        signals.push_value(scope("peer-b"));

        let mut seen = Vec::new();
        while let Some(signal) = signals.pop() {
            seen.push(signal);
        }
        // Two peers' security (latest per peer), one scan failure
        // (latest), two ingress-drop markers (merged per class), one value
        // marker for both dirty scopes.
        assert_eq!(seen.len(), 6);
        let security_a = seen
            .iter()
            .find(|signal| matches!(signal, HostSignal::Security(peer, _) if peer == "peer-a"))
            .expect("peer-a security");
        assert!(matches!(
            security_a,
            HostSignal::Security(_, state) if state.bond == BondState::Bonded
        ));
        let control = seen
            .iter()
            .find(|signal| matches!(signal, HostSignal::IngressDrop(class, _) if *class == IngressClass::Control))
            .expect("control marker");
        assert!(matches!(control, HostSignal::IngressDrop(_, 5)));
        let (lost, drops) = signals.take_overflow();
        assert_eq!((lost, drops), (0, [0, 0, 0]));
    }

    /// X-R6 follow-up: a stalled pump (no pops) plus far more distinct
    /// value scopes than the cap keeps the queue constantly bounded, and
    /// every dirty scope is still delivered — one shared marker, bounded
    /// batches, requeued while scopes remain. Nothing is stranded and
    /// nothing is silently dropped.
    #[test]
    fn distinct_value_scopes_stay_bounded_and_all_delivered() {
        let signals = Signals::default();
        let scopes = SIGNALS_CAP + 5000;
        for index in 0..scopes {
            signals.push_value(scope(&format!("peer-{index:05}")));
        }
        // Stalled: the whole burst costs one queue slot.
        assert_eq!(signals.queue_len(), 1);
        assert_eq!(signals.dirty_len(), scopes);
        // Pump simulation: each marker drains one bounded batch and
        // requeues while scopes remain; the queue never grows back.
        let mut delivered = HashSet::new();
        let mut markers = 0usize;
        while let Some(signal) = signals.pop() {
            assert!(
                matches!(signal, HostSignal::Values),
                "only the value marker waits while value scopes drain"
            );
            markers += 1;
            let batch = signals.take_value_batch(VALUE_SCOPE_BATCH);
            assert!(
                !batch.is_empty() && batch.len() <= VALUE_SCOPE_BATCH,
                "bounded non-empty batch per marker turn"
            );
            for scope in batch {
                assert!(delivered.insert(scope), "a scope drained twice");
            }
            signals.requeue_values_if_dirty();
            assert!(
                signals.queue_len() <= 1,
                "queue grew back mid-drain: {}",
                signals.queue_len()
            );
        }
        assert_eq!(delivered.len(), scopes, "every dirty scope drained");
        assert_eq!(signals.dirty_len(), 0, "no scope stranded dirty");
        assert!(
            markers * VALUE_SCOPE_BATCH >= scopes,
            "batching covered every scope in {markers} marker turns"
        );
        let (lost, drops) = signals.take_overflow();
        assert_eq!((lost, drops), (0, [0, 0, 0]));
    }

    /// A batch that stays busy is pushed behind the scopes still waiting.
    /// Hash-set iteration used to poll the same 32 and never reach the rest.
    #[test]
    fn busy_scopes_past_one_batch_each_get_a_turn() {
        let signals = Signals::default();
        let count = VALUE_SCOPE_BATCH + 8;
        let scopes: Vec<_> = (0..count)
            .map(|index| scope(&format!("peer-{index:05}")))
            .collect();
        for scope_key in &scopes {
            signals.push_value(scope_key.clone());
        }
        // Deadline and security sit behind the first value batch. They must
        // run before the requeued marker, while some scopes are still unseen.
        signals.push(HostSignal::ScanDeadlines);
        signals.push(security("peer-security", BondState::Bonded));
        let mut seen = HashSet::new();
        let mut saw_deadline = false;
        let mut saw_security = false;
        for _ in 0..(count + 2) {
            let signal = signals.pop().expect("queued signal");
            match signal {
                HostSignal::Values => {
                    let batch = signals.take_value_batch(VALUE_SCOPE_BATCH);
                    assert!(!batch.is_empty() && batch.len() <= VALUE_SCOPE_BATCH);
                    for scope_key in batch {
                        seen.insert(scope_key.clone());
                        signals.push_value(scope_key);
                    }
                    signals.requeue_values_if_dirty();
                }
                HostSignal::ScanDeadlines => {
                    assert!(
                        seen.len() < count,
                        "the deadline waited until every scope had a turn"
                    );
                    saw_deadline = true;
                }
                HostSignal::Security(_, state) => {
                    assert_eq!(state.bond, BondState::Bonded);
                    assert!(
                        seen.len() < count,
                        "security waited until every scope had a turn"
                    );
                    saw_security = true;
                }
                _ => panic!("unexpected signal while rotating busy scopes"),
            }
            if seen.len() == count && saw_deadline && saw_security {
                break;
            }
        }
        assert!(saw_deadline, "scan deadline never ran");
        assert!(saw_security, "security never ran");
        assert_eq!(
            seen.len(),
            count,
            "a busy first batch kept {count} scopes from all being polled; saw {}",
            seen.len()
        );
    }
}
