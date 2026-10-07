//! Owned connection-parameter watch for one connection.
//!
//! Windows 11 build 22000 reports observed interval, latency, and
//! supervision timeout. This route owns that read and the later reports
//! for one renderer. The values stay in microseconds here; the public
//! projection converts them to milliseconds. BlueZ and CoreBluetooth still
//! answer `capability.unsupported` from the core. Older Windows answers
//! `capability.unavailable`. The route exists so that refusal is the
//! platform's, not a missing command.

use super::*;

pub(super) struct ConnectionParameterWatch {
    task: TauriJoinHandle<()>,
}

impl Drop for ConnectionParameterWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ParameterReport<'a> {
    peer_id: &'a str,
    connection_id: &'a str,
    connection_generation: &'a str,
    sequence: u64,
    interval_us: u32,
    latency: u16,
    supervision_timeout_us: u32,
    observed_at: u64,
}

fn parameter_value(report: ParameterReport<'_>) -> IpcValue {
    object([
        ("kind", string("parameters")),
        ("peerId", string(report.peer_id)),
        ("connectionId", string(report.connection_id)),
        ("connectionGeneration", string(report.connection_generation)),
        ("sequence", IpcValue::Number(Number::from(report.sequence))),
        ("ordinal", IpcValue::Number(Number::from(report.sequence))),
        (
            "intervalUs",
            IpcValue::Number(Number::from(report.interval_us)),
        ),
        ("latency", IpcValue::Number(Number::from(report.latency))),
        (
            "supervisionTimeoutUs",
            IpcValue::Number(Number::from(report.supervision_timeout_us)),
        ),
        (
            "observedAtMonotonicMs",
            IpcValue::Number(Number::from(report.observed_at)),
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
    /// Observed connection parameters for the lease holding the link.
    /// Interval and supervision timeout are microseconds.
    pub(super) async fn read_connection_parameters(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let connection = self
            .connection(caller, &payload, "tauri.connection-parameters")
            .await?;
        let authority = self.ensure_authority().await?;
        let measured = authority
            .connection_parameters(&connection.peer_id, &connection.lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([
            (
                "intervalUs",
                IpcValue::Number(Number::from(measured.interval_us)),
            ),
            ("latency", IpcValue::Number(Number::from(measured.latency))),
            (
                "supervisionTimeoutUs",
                IpcValue::Number(Number::from(measured.supervision_timeout_us)),
            ),
            ("connectionId", string(&connection.connection_id)),
            (
                "connectionGeneration",
                string(&connection.connection_generation),
            ),
        ]))
    }

    pub(super) async fn subscribe_connection_parameters(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(
            &payload,
            "parameterEventsHandle",
            "tauri.connection-parameters-handle",
        )?;
        let connection = self
            .connection(caller, &payload, "tauri.connection-parameters")
            .await?;
        let authority = self.ensure_authority().await?;
        let mut receiver = authority.connection_parameter_events();
        let mut lifecycle = authority.lifecycle_events();
        let measured = authority
            .connection_parameters(&connection.peer_id, &connection.lease, ctl)
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
                    "tauri.connection-parameters-owner",
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
        let interval_us = measured.interval_us;
        let latency = measured.latency;
        let supervision_timeout_us = measured.supervision_timeout_us;
        let (started, start) = tokio::sync::oneshot::channel();
        let task = tauri::async_runtime::spawn(async move {
            if start.await.is_err() {
                return;
            }
            let mut sequence = 1u64;
            let initial = parameter_value(ParameterReport {
                peer_id: &peer_id,
                connection_id: &connection_id,
                connection_generation: &public_generation,
                sequence,
                interval_us,
                latency,
                supervision_timeout_us,
                observed_at,
            });
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
                                            "tauri.connection-parameters-sequence",
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
                                let value = parameter_value(ParameterReport {
                                    peer_id: &peer_id,
                                    connection_id: &connection_id,
                                    connection_generation: &public_generation,
                                    sequence,
                                    interval_us: event.interval_us,
                                    latency: event.latency,
                                    supervision_timeout_us: event.supervision_timeout_us,
                                    observed_at: observed,
                                });
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
                                    "tauri.connection-parameters-lag",
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
        let watch = ConnectionParameterWatch { task };
        {
            let mut state = self.inner.lock().await;
            let owner = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-parameters-owner",
                )
            })?;
            if owner.retired {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-parameters-owner",
                ));
            }
            if owner.parameter_watches.contains_key(&handle)
                || owner.parameter_releases.contains(&handle)
            {
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolViolation,
                    "connection",
                    "tauri.connection-parameters-duplicate",
                ));
            }
            if owner.parameter_watches.len() + owner.parameter_releases.len() >= MAX_PENDING_EVENTS
            {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "stream",
                    "tauri.connection-parameters-quota",
                ));
            }
            owner.parameter_watches.insert(handle.clone(), watch);
        }
        started.send(()).map_err(|_| {
            DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation,
                "connection",
                "tauri.connection-parameters-start",
            )
        })?;
        Ok(object([
            ("handle", string(handle)),
            ("intervalUs", IpcValue::Number(Number::from(interval_us))),
            ("latency", IpcValue::Number(Number::from(latency))),
            (
                "supervisionTimeoutUs",
                IpcValue::Number(Number::from(supervision_timeout_us)),
            ),
            ("connectionId", string(response_connection_id)),
            ("connectionGeneration", string(response_public_generation)),
        ]))
    }

    pub(super) async fn unsubscribe_connection_parameters(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(
            &payload,
            "parameterEventsHandle",
            "tauri.connection-parameters-unsubscribe",
        )?;
        let key = caller_key(caller);
        let mut state = self.inner.lock().await;
        let owner = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "connection",
                "tauri.connection-parameters-owner",
            )
        })?;
        if owner.parameter_watches.remove(&handle).is_some() {
            owner.parameter_releases.insert(handle);
            return Ok(released());
        }
        if owner.parameter_releases.contains(&handle) {
            return Ok(released());
        }
        Err(DispatchError::new(
            BleErrorCode::OwnershipDenied,
            "connection",
            "tauri.connection-parameters-unsubscribe-owner",
        ))
    }
}
