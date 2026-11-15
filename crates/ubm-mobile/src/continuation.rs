//! Mobile adapter to the one shared native continuation state machine.
use crate::{MobileHost, MobileSession, wire};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
pub use ubm_desktop::continuation::NativeContinuation;
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, Result, envelope,
};
use ubm_desktop::continuation_journal::JournalRegistry;

static TERMINAL_EVIDENCE_MAXIMUM_BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Production binding authority, available before a platform radio is installed.
pub fn process_recording_registry() -> Arc<JournalRegistry> {
    static REGISTRY: std::sync::OnceLock<Arc<JournalRegistry>> = std::sync::OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(JournalRegistry::default())))
}

/// Aggregate quota selected by trusted native bootstrap before the process
/// host opens.  It is never sourced from a wire request; absence means the
/// durable producer remains unavailable.
pub fn configure_terminal_evidence_maximum_bytes(
    maximum_bytes: u64,
) -> std::result::Result<(), &'static str> {
    if maximum_bytes == 0 {
        return Err("terminal evidence maximum must be positive");
    }
    match TERMINAL_EVIDENCE_MAXIMUM_BYTES.set(maximum_bytes) {
        Ok(()) => Ok(()),
        Err(_) if terminal_evidence_maximum_bytes() == Some(maximum_bytes) => Ok(()),
        Err(_) => Err("terminal evidence maximum already configured"),
    }
}

pub fn terminal_evidence_maximum_bytes() -> Option<u64> {
    TERMINAL_EVIDENCE_MAXIMUM_BYTES.get().copied()
}

/// Trusted private-store context access for a recording without a live owner.
pub fn recording_ordered_offline(operation: String, id: String, completion: crate::Completion) {
    let registry = process_recording_registry();
    ubm_desktop::executor::desktop_runtime().spawn(async move {
        let result = ubm_desktop::continuation_journal::run_blocking_result(move || {
            match operation.as_str() {
                "context" => registry.ordered_context_offline(&id),
                "status" => registry.ordered_status_offline(&id),
                "stop" => registry.stop_ordered_host_offline(&id),
                _ => return Err(serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.ordered-offline","detail":"unknown operation"})),
            }.map_err(ubm_desktop::continuation::recording_failure)
        })
        .await;
        completion(envelope(result));
    });
}

/// Closed, path-free controls shared by JNI/UniFFI and the installed owner.
/// Call on a native worker: SQLite operations are synchronous.
pub fn recording_control(
    registry: &JournalRegistry,
    engine: Option<&NativeContinuation>,
    operation: &str,
    id: &str,
    token: &str,
    max_items: u32,
    max_bytes: u32,
) -> String {
    use ubm_desktop::continuation::recording_failure;
    if !matches!(
        operation,
        "status" | "prepare" | "acknowledge" | "stop" | "clear"
    ) {
        return envelope(Err(
            json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording","detail":"unknown recording control"}),
        ));
    }
    if operation == "stop" {
        return envelope(match engine {
            Some(engine) => engine.recording_stop(id),
            None => registry
                .get(id)
                .map_err(recording_failure)
                .and_then(|journal| {
                    ubm_desktop::continuation_journal::stop_legacy_recording(&journal)
                }),
        });
    }
    let result = registry
        .get(id)
        .and_then(|journal| match operation {
            "status" => journal.status(),
            "prepare" => journal.prepare(max_items, max_bytes),
            "acknowledge" => journal.acknowledge(token),
            "clear" => journal.clear(),
            _ => unreachable!("validated above"),
        })
        .map_err(recording_failure);
    envelope(result)
}

struct HostAdapter(Weak<crate::host::HostInner>);
struct SessionAdapter {
    host: Weak<crate::host::HostInner>,
    state: Arc<crate::session::SessionState>,
}

fn gone() -> Value {
    json!({"code":"lifecycle.destroyed","domain":"restoration","operation":"continuation","detail":"native host or session is gone"})
}

impl ContinuationHost for HostAdapter {
    fn canonical_peer(&self, peer: &str) -> String {
        let separators: Option<(u8, &[usize])> = match peer.len() {
            17 => Some((b':', &[2, 5, 8, 11, 14])),
            36 => Some((b'-', &[8, 13, 18, 23])),
            _ => None,
        };
        if separators.is_some_and(|(separator, positions)| {
            peer.bytes().enumerate().all(|(index, byte)| {
                if positions.contains(&index) {
                    byte == separator
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        }) {
            peer.to_ascii_uppercase()
        } else {
            peer.to_owned()
        }
    }

    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        let inner = self.0.upgrade().ok_or_else(gone)?;
        let _admission = inner
            .continuation_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.continuation_closed.load(Ordering::SeqCst) {
            return Err(gone());
        }
        let session = MobileHost {
            inner: Arc::clone(&inner),
        }
        .open_native_session("unified-ble-manager/continuation")
        .map_err(|error| Value::Object(wire::error_object(&error)))?;
        let state = inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session.id())
            .cloned()
            .ok_or_else(gone)?;
        Ok(Arc::new(SessionAdapter {
            host: self.0.clone(),
            state,
        }))
    }
}

impl SessionAdapter {
    fn resolve(&self) -> Result<MobileSession> {
        let inner = self.host.upgrade().ok_or_else(gone)?;
        Ok(MobileSession::new(inner, Arc::clone(&self.state)))
    }
}

impl ContinuationSession for SessionAdapter {
    fn legacy_claim_compatible(&self) -> bool {
        true
    }
    fn seal_collection(&self) -> Result<()> {
        if self.resolve()?.seal_continuation_collection() {
            Ok(())
        } else {
            Err(
                json!({"code":"lifecycle.invalid-state", "domain":"restoration", "operation":"continuation.seal", "detail":"scan ingress is pending"}),
            )
        }
    }
    fn collection_sealed(&self) -> bool {
        self.resolve()
            .is_ok_and(|session| session.continuation_collection_sealed())
    }
    fn attach_journal(
        &self,
        journal: Arc<ubm_desktop::continuation_journal::ContinuationJournal>,
        context: Value,
    ) -> Result<()> {
        self.resolve()?
            .attach_continuation_journal(journal, context)
    }
    fn register_journal_consumer(&self, consumer: &str, metadata: Value) -> Result<()> {
        self.resolve()?
            .register_continuation_consumer(consumer, metadata)
    }
    fn observe(
        &self,
        consumer: &str,
        matcher: ubm_desktop::continuation_outbox::RecordMatcher,
    ) -> Result<ubm_desktop::continuation_outbox::Observation> {
        self.resolve()?.observe_continuation(consumer, matcher).map_err(|detail|json!({"code":"lifecycle.invalid-state","domain":"restoration","operation":"continuation","detail":detail}))
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            if matches!(
                op,
                "connection.connect"
                    | "connection.request-mtu"
                    | "gatt.discover"
                    | "gatt.subscribe"
                    | "gatt.write"
            ) && self
                .host
                .upgrade()
                .is_none_or(|host| host.continuation_closed.load(Ordering::SeqCst))
            {
                return envelope(Err(gone()));
            }
            match self.resolve() {
                Ok(session) => session.call(op, args).await,
                Err(error) => envelope(Err(error)),
            }
        })
    }
    fn legacy_drain(
        &self,
        evidence: &ubm_desktop::continuation_outbox::LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Value>> {
        Some(
            self.state
                .outbox
                .drain_legacy_preserving(
                    evidence,
                    items as usize,
                    bytes as usize,
                    ubm_desktop::continuation::project_521_record,
                )
                .map_err(ubm_desktop::continuation::legacy_evidence_error),
        )
    }
    fn claim_drain_v1(
        &self,
        evidence: &ubm_desktop::continuation_outbox::LegacyEvidenceOwner,
        max_items: u32,
        max_bytes: u32,
    ) -> Option<Result<Option<ubm_desktop::continuation_outbox::DrainClaim>>> {
        Some(
            self.state
                .outbox
                .claim_drain_projected(
                    evidence,
                    max_items as usize,
                    max_bytes as usize,
                    ubm_desktop::continuation::project_521_record,
                )
                .map_err(ubm_desktop::continuation::legacy_evidence_error),
        )
    }
    fn claim_drain(
        &self,
        items: u32,
        bytes: u32,
    ) -> Result<Option<ubm_desktop::continuation_outbox::DrainClaim>> {
        self.state
            .outbox
            .claim_drain(items as usize, bytes as usize)
            .map_err(ubm_desktop::continuation::continuation_drain_error)
    }
    fn has_retained_tail(&self) -> Result<bool> {
        self.state
            .outbox
            .has_retained_tail()
            .map_err(ubm_desktop::continuation::continuation_drain_error)
    }
    fn acknowledge_drain(&self, id: u64) -> Result<()> {
        if self.state.outbox.acknowledge_drain(id) {
            Ok(())
        } else {
            Err(
                json!({"code":"lifecycle.invalid-state","domain":"restoration","operation":"continuation","detail":"continuation outbox claim mismatch"}),
            )
        }
    }
}

impl MobileHost {
    /// Read one bounded page from a V2 peer-source failure record emitted by
    /// the continuation outbox. The record carries the exact source-time
    /// route; the central rejects stale/replaced routes and never advances the
    /// source on a read.
    pub fn continuation_read_peer_failure_recovery(
        &self,
        recovery_record: &str,
        offset: u64,
        completion: crate::Completion,
    ) {
        const OP: &str = "continuation.peer-source-failure.read";
        let parsed = super::host::parse_peer_source_failure_recovery(recovery_record, OP);
        let central = self.inner.central.clone();
        self.inner.runtime.spawn(async move {
            let result = match parsed {
                Ok((descriptor, peer_id, selector, consumer, recipient_lease)) => central
                    .read_peer_notification_failure_recovery(
                        &descriptor,
                        &peer_id,
                        &selector,
                        &consumer,
                        &recipient_lease,
                        offset,
                    )
                    .await
                    .map(|chunk| {
                        json!({
                            "offset": chunk.offset.to_string(),
                            "nextOffset": chunk.next_offset.to_string(),
                            "bytesB64": crate::wire::encode_base64(&chunk.bytes),
                            "done": chunk.done,
                        })
                    }),
                Err(error) => Err(error),
            };
            completion(envelope(
                result.map_err(|error| Value::Object(wire::error_object(&error))),
            ));
        });
    }

    /// Acknowledge only after the caller has read every contiguous page. The
    /// central holds the source until every source-time recipient satisfies
    /// this acknowledgement; one V2 consumer cannot release another.
    pub fn continuation_acknowledge_peer_failure_recovery(
        &self,
        recovery_record: &str,
        completion: crate::Completion,
    ) {
        const OP: &str = "continuation.peer-source-failure.acknowledge";
        let parsed = super::host::parse_peer_source_failure_recovery(recovery_record, OP);
        let central = self.inner.central.clone();
        self.inner.runtime.spawn(async move {
            let result = match parsed {
                Ok((descriptor, peer_id, selector, consumer, recipient_lease)) => central
                    .ack_peer_notification_failure_recovery(
                        &descriptor,
                        &peer_id,
                        &selector,
                        &consumer,
                        &recipient_lease,
                    )
                    .await
                    .map(|()| json!({})),
                Err(error) => Err(error),
            };
            completion(envelope(
                result.map_err(|error| Value::Object(wire::error_object(&error))),
            ));
        });
    }

    pub fn continuation_recording_control(
        &self,
        operation: &str,
        id: &str,
        token: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> String {
        recording_control(
            &self.inner.recording_registry,
            self.inner.continuation.get(),
            operation,
            id,
            token,
            max_items,
            max_bytes,
        )
    }
    pub fn continuation_configure_recording_directory(&self, path: &std::path::Path) -> String {
        envelope(self.continuation().configure_recording_directory(path))
    }
    /// Returns a spool only for a recording the host has already configured.
    /// The caller never supplies or derives a filesystem path.
    pub fn notification_failure_spool(
        &self,
        recording_id: &str,
    ) -> std::result::Result<
        Arc<ubm_desktop::continuation_journal::JournalAppleNotificationFailureSpool>,
        ubm_desktop::continuation_journal::JournalError,
    > {
        self.inner
            .recording_registry
            .get(recording_id)?
            .apple_notification_failure_spool()
    }
    /// Opens the Apple failure spool with the explicitly separate budget the
    /// host negotiated for overflow retention.
    pub fn notification_failure_spool_with_quota(
        &self,
        recording_id: &str,
        quota: ubm_desktop::continuation_journal::JournalQuota,
    ) -> std::result::Result<
        Arc<ubm_desktop::continuation_journal::JournalAppleNotificationFailureSpool>,
        ubm_desktop::continuation_journal::JournalError,
    > {
        self.inner
            .recording_registry
            .get(recording_id)?
            .apple_notification_failure_spool_with_quota(quota)
    }
    pub fn continuation_recording_status(&self, id: &str) -> String {
        envelope(self.continuation().recording_status(id))
    }
    pub fn continuation_recording_prepare(
        &self,
        id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> String {
        envelope(
            self.continuation()
                .recording_prepare(id, max_items, max_bytes),
        )
    }
    pub fn continuation_recording_acknowledge(&self, id: &str, token: &str) -> String {
        envelope(self.continuation().recording_acknowledge(id, token))
    }
    pub fn continuation_recording_stop(&self, id: &str) -> String {
        envelope(self.continuation().recording_stop(id))
    }
    pub fn continuation_recording_clear(&self, id: &str) -> String {
        envelope(self.continuation().recording_clear(id))
    }
    pub fn continuation_reserve_declaration(&self, declaration: &str) -> String {
        envelope(self.continuation().reserve_declaration(declaration))
    }
    pub fn continuation_commit_declaration(&self, token: &str) -> String {
        envelope(self.continuation().commit_declaration(token))
    }
    pub fn continuation_cancel_declaration(&self, token: &str) -> String {
        envelope(self.continuation().cancel_declaration(token))
    }
    pub fn continuation_seed_declaration(&self, declaration: &str) -> String {
        envelope(self.continuation().seed_declaration(declaration))
    }
    pub fn continuation(&self) -> NativeContinuation {
        self.inner
            .continuation
            .get_or_init(|| {
                NativeContinuation::new_with_recording_registry(
                    Arc::new(HostAdapter(Arc::downgrade(&self.inner))),
                    Arc::clone(&self.inner.recording_registry),
                )
            })
            .clone()
    }
    pub fn continuation_declaration_replacement_failure(
        &self,
        declaration: &str,
    ) -> Option<String> {
        self.continuation()
            .declaration_replacement_failure(declaration)
    }
    pub fn continuation_describe_backlog(&self, completion: crate::Completion) {
        let executor = self.continuation();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.describe_backlog().await));
        });
    }
    pub fn continuation_execute(
        &self,
        peer: &str,
        declaration: &str,
        completion: crate::Completion,
    ) {
        if self.inner.continuation_closed.load(Ordering::SeqCst) {
            completion(envelope(Err(gone())));
            return;
        }
        let executor = self.continuation();
        let peer = peer.to_owned();
        let declaration = declaration.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.execute(&peer, &declaration).await));
        });
    }
    pub fn continuation_prepare_claim(
        &self,
        items: u32,
        bytes: u32,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.prepare_claim(items, bytes).await));
        });
    }
    pub fn continuation_acknowledge_claim(&self, token: &str, completion: crate::Completion) {
        let executor = self.continuation();
        let token = token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.acknowledge_claim(&token).await));
        });
    }
    /// Acknowledgement for the separately negotiated retained-prefix protocol.
    pub fn continuation_acknowledge_retained_claim_v1(
        &self,
        token: &str,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        let token = token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.acknowledge_claim_v1(&token).await));
        });
    }
    pub fn continuation_prepare_retained_claim_v1(
        &self,
        items: u32,
        bytes: u32,
        completion: crate::Completion,
    ) {
        self.continuation_prepare_claim_versioned(1, items, bytes, completion);
    }
    /// Additive V2 retained-prefix capability.  It is intentionally a new
    /// entrypoint so a 5.2.1 V1 decoder can never receive a V2 descriptor.
    pub fn continuation_prepare_retained_claim_v2(
        &self,
        items: u32,
        bytes: u32,
        completion: crate::Completion,
    ) {
        self.continuation_prepare_claim_versioned(2, items, bytes, completion);
    }
    pub fn continuation_acknowledge_retained_claim_v2(
        &self,
        token: &str,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        let token = token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.acknowledge_claim_v2(&token).await));
        });
    }
    pub fn continuation_retire_claim_receipt(&self, token: &str, completion: crate::Completion) {
        let executor = self.continuation();
        let token = token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(
                executor
                    .retire_claim_receipt(&token)
                    .await
                    .map(|()| json!({})),
            ));
        });
    }
    /// Page exact records omitted from a strict 5.2.1 legacy projection.
    /// `claim_token` is the original legacy claim token; success is an
    /// envelope containing `{records,more}`.
    pub fn continuation_legacy_evidence_page(
        &self,
        claim_token: &str,
        after_ordinal: u64,
        max_items: u32,
        max_bytes: u32,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        let claim_token = claim_token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(
                executor
                    .legacy_evidence_page(&claim_token, after_ordinal, max_items, max_bytes)
                    .await,
            ));
        });
    }
    /// Retire exactly one page record after the consumer accepts its original
    /// bytes. Absence remains a visible error rather than a false success.
    pub fn continuation_legacy_evidence_retire(
        &self,
        claim_token: &str,
        ordinal: u64,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        let claim_token = claim_token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(
                executor
                    .legacy_evidence_retire(&claim_token, ordinal)
                    .await
                    .map(|()| json!({})),
            ));
        });
    }
    pub fn continuation_prepare_claim_versioned(
        &self,
        version: u32,
        items: u32,
        bytes: u32,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        self.inner.runtime.spawn(async move {
            completion(envelope(
                executor
                    .prepare_claim_versioned(version, items, bytes)
                    .await,
            ));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_peer_canonicalization_is_limited_to_uuid_and_mac() {
        let host = HostAdapter(Weak::new());
        for peer in ["aa:bb:cc:dd:ee:ff", "abcdef01-2345-6789-abcd-ef0123456789"] {
            assert_eq!(host.canonical_peer(peer), peer.to_ascii_uppercase());
        }
        for peer in [
            "hci1/dev_aa_bb_cc_dd_ee_ff",
            "opaque-Peer",
            "not-a-uuid",
            "gg:bb:cc:dd:ee:ff",
        ] {
            assert_eq!(host.canonical_peer(peer), peer);
        }
    }
}
