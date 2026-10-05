//! Security IPC is a projection of the instantiated desktop authority, not a
//! second pairing owner. Operation tickets and native outcomes cross unchanged.
use super::*;
use ubm_desktop::{
    CancelPairingOutcome, PairOutcome, PairRequest, SecureConnections, SecurityState, UnpairOutcome,
};

pub(super) struct SecurityWatch {
    task: TauriJoinHandle<()>,
}

impl Drop for SecurityWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Host observation time, not an invented native event-capture timestamp.
// Buffered observations retain this time instead of being re-stamped on replay.
fn state_record(state: SecurityState, measured_at: u64) -> IpcValue {
    object([
        ("bond", string(state.bond.as_str())),
        ("encryption", string("unsupported")),
        ("authentication", string("unsupported")),
        ("secureConnections", string("unsupported")),
        (
            "pairingPossible",
            state
                .pairing_possible
                .map(IpcValue::Bool)
                .unwrap_or(IpcValue::Null),
        ),
        (
            "measuredAtMonotonicMs",
            IpcValue::Number(Number::from(measured_at)),
        ),
        (
            "limitations",
            IpcValue::Array(vec![object([
                ("code", string("desktop-security-measurement")),
                (
                    "explanation",
                    string(
                        "The native authority reports bond state and pairing availability; encryption, authentication and Secure Connections are not measured.",
                    ),
                ),
                (
                    "affectedGuarantee",
                    string("security measurement completeness"),
                ),
            ])]),
        ),
    ])
}

fn malformed(operation: &str) -> DispatchError {
    DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
}

impl BtleplugDispatcher {
    pub(super) async fn security_route(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let common = [
            "deadline",
            "budgetMs",
            "__expectedLeaseId",
            "__expectedLeaseGeneration",
        ];
        let fields = match command {
            "security.pair" => &[
                "peerId",
                "transport",
                "protection",
                "secureConnections",
                "ceremony",
            ][..],
            "security.watch.unsubscribe" => &["handle"][..],
            _ => &["peerId"][..],
        };
        if payload
            .keys()
            .any(|key| !common.contains(&key.as_str()) && !fields.contains(&key.as_str()))
        {
            return Err(malformed(command));
        }
        let key = caller_key(caller);
        let lease = expected_lease(&payload, "tauri.security-lease")?;
        if command == "security.watch.unsubscribe" {
            let handle = required_string(&payload, "handle", command)?;
            let mut state = self.inner.lock().await;
            let owner = state
                .callers
                .get_mut(&key)
                .filter(|owner| lease_matches(owner, &lease))
                .ok_or_else(|| DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", command))?;
            if owner.security_watches.remove(&handle).is_some() {
                owner.security_watch_releases.insert(handle);
            } else if !owner.security_watch_releases.contains(&handle) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "ipc",
                    "security.watch.unsubscribe-owner",
                ));
            }
            return Ok(released());
        }
        let peer = required_string(&payload, "peerId", command)?;
        let authority = self.ensure_authority().await?;
        if command == "security.watch.subscribe" {
            let mut receiver = authority.security_events();
            // Verify this peer/mechanism before granting a watch. Capturing the
            // receiver first retains changes racing the state admission read.
            let initial = authority
                .security_state(&peer, ctl)
                .await
                .map_err(|error| DispatchError::from_core(&error))?;
            let initial_observed_at = self
                .started_at
                .elapsed()
                .as_millis()
                .min(MAX_SAFE_INTEGER as u128) as u64;
            let mut buffered = std::collections::VecDeque::new();
            // The read has no revision tying its native capture to an event.
            // A later response can contain an older snapshot, or vice versa.
            // Retain every sequenced racing event and prefer that ordered
            // source; never invent an ordering for the unsequenced snapshot.
            for _ in 0..MAX_PENDING_EVENTS {
                match receiver.try_recv() {
                    Ok(event) if event.peer_id == peer => buffered.push_back((
                        event,
                        self.started_at
                            .elapsed()
                            .as_millis()
                            .min(MAX_SAFE_INTEGER as u128) as u64,
                    )),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => {
                        return Err(DispatchError::new(
                            BleErrorCode::PlatformTransport,
                            "stream",
                            "security.watch.initial-events",
                        )
                        .platform(error.to_string()));
                    }
                }
            }
            if !receiver.is_empty() {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "stream",
                    "security.watch.initial-events",
                )
                .platform("Initial security-event replay exceeds the bounded admission capacity"));
            }
            let handle = self.id("security-watch");
            let stream = handle.clone();
            let dispatcher = self.clone();
            let task_key = key.clone();
            let task_lease = lease.clone();
            let (ready, start) = tokio::sync::oneshot::channel();
            let task = tauri::async_runtime::spawn(async move {
                if start.await.is_err() {
                    return;
                }
                // Snapshot and source events form this watch's observation
                // sequence; the central's counter excludes the snapshot and
                // includes unrelated peers, so it cannot index this stream.
                let mut sequence = 0u64;
                if buffered.is_empty() {
                    sequence = 1;
                    let initial = object([
                        ("kind", string("state")),
                        ("peerId", string(&peer)),
                        ("sequence", IpcValue::Number(Number::from(sequence))),
                        ("state", state_record(initial, initial_observed_at)),
                    ]);
                    if let Err(error) = dispatcher
                        .emit(
                            &task_key,
                            Some((&task_lease.0, &task_lease.1)),
                            &stream,
                            initial,
                        )
                        .await
                    {
                        if let Err(terminal_error) = dispatcher
                            .terminal(
                                &task_key,
                                (&task_lease.0, &task_lease.1),
                                &stream,
                                "source-failed",
                                Some(&error),
                            )
                            .await
                        {
                            eprintln!(
                                "UBM initial security watch delivery failed: {error:?}; terminal: {terminal_error:?}"
                            );
                        }
                        return;
                    }
                }
                loop {
                    let received = match buffered.pop_front() {
                        Some((event, observed_at)) => Ok((event, Some(observed_at))),
                        None => receiver.recv().await.map(|event| (event, None)),
                    };
                    match received {
                        Ok((event, observed_at)) if event.peer_id == peer => {
                            let delivery = async {
                                sequence = sequence
                                    .checked_add(1)
                                    .filter(|value| *value <= MAX_SAFE_INTEGER)
                                    .ok_or_else(|| {
                                        DispatchError::new(
                                            BleErrorCode::StreamQuota,
                                            "stream",
                                            "security.watch.sequence",
                                        )
                                    })?;
                                let value = object([
                                    ("kind", string("state")),
                                    ("peerId", string(&event.peer_id)),
                                    ("sequence", IpcValue::Number(Number::from(sequence))),
                                    (
                                        "state",
                                        state_record(
                                            event.state,
                                            observed_at.unwrap_or_else(|| {
                                                dispatcher
                                                    .started_at
                                                    .elapsed()
                                                    .as_millis()
                                                    .min(MAX_SAFE_INTEGER as u128)
                                                    as u64
                                            }),
                                        ),
                                    ),
                                ]);
                                dispatcher
                                    .emit(
                                        &task_key,
                                        Some((&task_lease.0, &task_lease.1)),
                                        &stream,
                                        value,
                                    )
                                    .await
                            }
                            .await;
                            if let Err(error) = delivery {
                                if let Err(terminal_error) = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&task_lease.0, &task_lease.1),
                                        &stream,
                                        "source-failed",
                                        Some(&error),
                                    )
                                    .await
                                {
                                    eprintln!(
                                        "UBM security watch delivery failed: {error:?}; terminal: {terminal_error:?}"
                                    );
                                }
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let reason = if matches!(
                                error,
                                tokio::sync::broadcast::error::RecvError::Lagged(_)
                            ) {
                                "overflow"
                            } else {
                                "source-failed"
                            };
                            let failure = DispatchError::new(
                                BleErrorCode::PlatformTransport,
                                "stream",
                                "security.watch",
                            )
                            .platform(error.to_string());
                            if let Err(error) = dispatcher
                                .terminal(
                                    &task_key,
                                    (&task_lease.0, &task_lease.1),
                                    &stream,
                                    reason,
                                    Some(&failure),
                                )
                                .await
                            {
                                eprintln!("UBM security watch terminal failed: {error:?}");
                            }
                            break;
                        }
                    }
                }
            });
            let watch = SecurityWatch { task };
            let mut state = self.inner.lock().await;
            let owner = state
                .callers
                .get_mut(&key)
                .filter(|owner| !owner.retired && lease_matches(owner, &lease))
                .ok_or_else(|| DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", command))?;
            if owner.security_watches.len() + owner.security_watch_releases.len()
                >= MAX_PENDING_EVENTS
            {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "stream",
                    "security.watch.quota",
                ));
            }
            owner.security_watches.insert(handle.clone(), watch);
            ready.send(()).map_err(|_| {
                DispatchError::new(BleErrorCode::LifecycleInvariantViolation, "ipc", command)
            })?;
            return Ok(object([("handle", string(handle))]));
        }
        let now = || {
            self.started_at
                .elapsed()
                .as_millis()
                .min(MAX_SAFE_INTEGER as u128) as u64
        };
        match command {
            "security.state" => Ok(object([(
                "state",
                state_record(
                    authority
                        .security_state(&peer, ctl)
                        .await
                        .map_err(|error| DispatchError::from_core(&error))?,
                    now(),
                ),
            )])),
            "security.pair" => {
                for (field, admitted) in [
                    ("transport", &["le", "auto"][..]),
                    ("protection", &["system-default"][..]),
                    ("ceremony", &["system"][..]),
                ] {
                    match payload.get(field) {
                        None => {}
                        Some(IpcValue::String(value)) if admitted.contains(&value.as_str()) => {}
                        Some(IpcValue::String(_)) | Some(IpcValue::Object(_)) => {
                            return Err(DispatchError::new(
                                BleErrorCode::CapabilityUnsupported,
                                "capability",
                                command,
                            )
                            .platform(format!(
                                "The instantiated desktop authority cannot apply {field}"
                            )));
                        }
                        _ => return Err(malformed(command)),
                    }
                }
                let secure_connections = match payload.get("secureConnections") {
                    None => None,
                    Some(IpcValue::String(value)) if value == "prefer" => None,
                    Some(IpcValue::String(value)) if value == "require" => {
                        Some(SecureConnections::Require)
                    }
                    Some(IpcValue::String(value)) if value == "disallow" => {
                        Some(SecureConnections::Disallow)
                    }
                    _ => return Err(malformed(command)),
                };
                let answer = authority
                    .pair(
                        &peer,
                        PairRequest {
                            secure_connections,
                            generation_controller: None,
                        },
                        ctl,
                    )
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                let result = match answer {
                    PairOutcome::Paired(state) => object([
                        ("outcome", string("paired")),
                        ("state", state_record(state, now())),
                    ]),
                    PairOutcome::AlreadyPaired(state) => object([
                        ("outcome", string("already-paired")),
                        ("state", state_record(state, now())),
                    ]),
                    PairOutcome::Rejected(reason) => object([
                        ("outcome", string("rejected")),
                        ("reason", reason.map(string).unwrap_or(IpcValue::Null)),
                    ]),
                    PairOutcome::Cancelled => object([("outcome", string("cancelled"))]),
                };
                Ok(object([("result", result)]))
            }
            "security.cancel-pairing" => {
                let answer = authority
                    .cancel_pairing(&peer, ctl)
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                let result = match answer {
                    CancelPairingOutcome::Cancelled => object([("outcome", string("cancelled"))]),
                    CancelPairingOutcome::NotPairing => {
                        object([("outcome", string("not-pairing"))])
                    }
                    CancelPairingOutcome::Paired => object([("outcome", string("paired"))]),
                    CancelPairingOutcome::Rejected(reason) => object([
                        ("outcome", string("rejected")),
                        ("reason", reason.map(string).unwrap_or(IpcValue::Null)),
                    ]),
                };
                Ok(object([("result", result)]))
            }
            "security.unpair" => {
                let answer = authority
                    .unpair(&peer, ctl)
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                Ok(object([(
                    "result",
                    object([(
                        "outcome",
                        string(match answer {
                            UnpairOutcome::Unpaired => "unpaired",
                            UnpairOutcome::AlreadyUnpaired => "already-unpaired",
                        }),
                    )]),
                )]))
            }
            _ => Err(malformed(command)),
        }
    }
}
