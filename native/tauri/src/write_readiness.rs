//! Owned write-without-response readiness watch for one connection.
//!
//! The desktop core already probes `canSendWriteWithoutResponse` and
//! publishes later reports. This route owns that watch for one renderer:
//! the initial probe is the first value, later reports stay on this
//! connection's generation, and link loss ends the stream. Windows and
//! Linux still answer `capability.unsupported` from the core; the route
//! exists so the refusal is the platform's, not a missing command.
use super::*;

pub(super) struct WriteReadinessWatch {
    task: TauriJoinHandle<()>,
}

impl Drop for WriteReadinessWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn readiness_value(
    peer_id: &str,
    connection_id: &str,
    connection_generation: &str,
    sequence: u64,
    ready: bool,
    observed_at: u64,
) -> IpcValue {
    object([
        ("kind", string("readiness")),
        ("peerId", string(peer_id)),
        ("connectionId", string(connection_id)),
        ("connectionGeneration", string(connection_generation)),
        ("sequence", IpcValue::Number(Number::from(sequence))),
        ("ordinal", IpcValue::Number(Number::from(sequence))),
        ("ready", IpcValue::Bool(ready)),
        (
            "observedAtMonotonicMs",
            IpcValue::Number(Number::from(observed_at)),
        ),
    ])
}

fn link_ended(kind: &ubm_desktop::LifecycleKind) -> bool {
    matches!(
        kind,
        ubm_desktop::LifecycleKind::LinkLost
            | ubm_desktop::LifecycleKind::Released { .. }
            | ubm_desktop::LifecycleKind::AdapterLost
    )
}

impl BtleplugDispatcher {
    pub(super) async fn subscribe_write_readiness(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(
            &payload,
            "writeReadinessHandle",
            "tauri.write-readiness-handle",
        )?;
        let connection = self
            .connection(caller, &payload, "tauri.write-readiness")
            .await?;
        let authority = self.ensure_authority().await?;
        let mut receiver = authority.write_readiness_events();
        let mut lifecycle = authority.lifecycle_events();
        let ready = authority
            .write_readiness(&connection.peer_id, &connection.lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let observed_at = self
            .started_at
            .elapsed()
            .as_millis()
            .min(MAX_SAFE_INTEGER as u128) as u64;
        let key = caller_key(caller);
        let lease = {
            let state = self.inner.lock().await;
            let owner = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.write-readiness-owner",
                )
            })?;
            (owner.lease_id.clone(), owner.lease_generation.clone())
        };
        let peer_id = connection.peer_id.clone();
        let connection_id = connection.connection_id.clone();
        let response_connection_id = connection_id.clone();
        let connection_generation = connection.core_generation.clone();
        let public_generation = connection.connection_generation.clone();
        let response_public_generation = public_generation.clone();
        let stream = handle.clone();
        let dispatcher = self.clone();
        let task_key = key.clone();
        let (started, start) = tokio::sync::oneshot::channel();
        let task = tauri::async_runtime::spawn(async move {
            if start.await.is_err() {
                return;
            }
            let mut sequence = 1u64;
            let initial = readiness_value(
                &peer_id,
                &connection_id,
                &public_generation,
                sequence,
                ready,
                observed_at,
            );
            if let Err(error) = dispatcher
                .emit(&task_key, Some((&lease.0, &lease.1)), &stream, initial)
                .await
            {
                let _ = dispatcher
                    .terminal(
                        &task_key,
                        (&lease.0, &lease.1),
                        &stream,
                        "source-failed",
                        Some(&error),
                    )
                    .await;
                return;
            }
            loop {
                tokio::select! {
                    received = receiver.recv() => {
                        match received {
                            Ok(event)
                                if event.peer_id == peer_id
                                    && event.connection_generation.as_deref()
                                        == Some(connection_generation.as_str()) =>
                            {
                                sequence = match sequence.checked_add(1) {
                                    Some(next) if next <= MAX_SAFE_INTEGER => next,
                                    _ => {
                                        let failure = DispatchError::new(
                                            BleErrorCode::StreamQuota,
                                            "stream",
                                            "tauri.write-readiness-sequence",
                                        );
                                        let _ = dispatcher
                                            .terminal(
                                                &task_key,
                                                (&lease.0, &lease.1),
                                                &stream,
                                                "overflow",
                                                Some(&failure),
                                            )
                                            .await;
                                        break;
                                    }
                                };
                                let observed = dispatcher
                                    .started_at
                                    .elapsed()
                                    .as_millis()
                                    .min(MAX_SAFE_INTEGER as u128)
                                    as u64;
                                let value = readiness_value(
                                    &peer_id,
                                    &connection_id,
                                    &public_generation,
                                    sequence,
                                    event.ready,
                                    observed,
                                );
                                if let Err(error) = dispatcher
                                    .emit(
                                        &task_key,
                                        Some((&lease.0, &lease.1)),
                                        &stream,
                                        value,
                                    )
                                    .await
                                {
                                    let _ = dispatcher
                                        .terminal(
                                            &task_key,
                                            (&lease.0, &lease.1),
                                            &stream,
                                            "source-failed",
                                            Some(&error),
                                        )
                                        .await;
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let failure = DispatchError::new(
                                    BleErrorCode::StreamQuota,
                                    "stream",
                                    "tauri.write-readiness-lag",
                                );
                                let _ = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&lease.0, &lease.1),
                                        &stream,
                                        "overflow",
                                        Some(&failure),
                                    )
                                    .await;
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&lease.0, &lease.1),
                                        &stream,
                                        "source-failed",
                                        None,
                                    )
                                    .await;
                                break;
                            }
                        }
                    }
                    ended = lifecycle.recv() => {
                        match ended {
                            Ok(event)
                                if event.peer_id == peer_id
                                    && link_ended(&event.kind)
                                    && event.connection_generation.as_deref()
                                        == Some(connection_generation.as_str()) =>
                            {
                                let _ = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&lease.0, &lease.1),
                                        &stream,
                                        "connection-lost",
                                        None,
                                    )
                                    .await;
                                break;
                            }
                            Ok(_) => {}
                            Err(_) => {
                                let _ = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&lease.0, &lease.1),
                                        &stream,
                                        "source-failed",
                                        None,
                                    )
                                    .await;
                                break;
                            }
                        }
                    }
                }
            }
        });
        let watch = WriteReadinessWatch { task };
        {
            let mut state = self.inner.lock().await;
            let owner = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.write-readiness-owner",
                )
            })?;
            if owner.retired {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.write-readiness-owner",
                ));
            }
            if owner.write_readiness_watches.contains_key(&handle)
                || owner.write_readiness_releases.contains(&handle)
            {
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolViolation,
                    "connection",
                    "tauri.write-readiness-duplicate",
                ));
            }
            if owner.write_readiness_watches.len() >= MAX_PENDING_EVENTS {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "stream",
                    "tauri.write-readiness-quota",
                ));
            }
            owner.write_readiness_watches.insert(handle.clone(), watch);
        }
        started.send(()).map_err(|_| {
            DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation,
                "connection",
                "tauri.write-readiness-start",
            )
        })?;
        Ok(object([
            ("handle", string(handle)),
            ("ready", IpcValue::Bool(ready)),
            ("connectionId", string(response_connection_id)),
            ("connectionGeneration", string(response_public_generation)),
        ]))
    }

    pub(super) async fn unsubscribe_write_readiness(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(
            &payload,
            "writeReadinessHandle",
            "tauri.write-readiness-unsubscribe",
        )?;
        let key = caller_key(caller);
        let mut state = self.inner.lock().await;
        let owner = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "connection",
                "tauri.write-readiness-owner",
            )
        })?;
        if owner.write_readiness_watches.remove(&handle).is_some() {
            owner.write_readiness_releases.insert(handle);
            return Ok(released());
        }
        if owner.write_readiness_releases.contains(&handle) {
            return Ok(released());
        }
        Err(DispatchError::new(
            BleErrorCode::OwnershipDenied,
            "connection",
            "tauri.write-readiness-unsubscribe-owner",
        ))
    }
}
