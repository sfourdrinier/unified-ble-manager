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

/// One parameter read performed before the watch is published.
///
/// A core ticket names one operation, so this read gets its own child
/// ticket. The child keeps the caller's absolute budget, and cancelling
/// the caller ticket cancels the child. The caller ticket stays pending
/// for the rest of subscribe.
async fn connection_parameters_within_request(
    authority: &dyn CoreAuthority,
    peer_id: &str,
    lease: &str,
    parent: &OpControl,
) -> Result<ubm_desktop::ObservedConnectionParameters, DispatchError> {
    let child = OpControl::new(parent.budget, OpTicket::new());
    let read = authority.connection_parameters(peer_id, lease, child.clone());
    finish_within_caller_control(parent, &child, read).await
}

async fn finish_within_caller_control<T>(
    parent: &OpControl,
    child: &OpControl,
    read: impl std::future::Future<Output = Result<T, ubm_desktop::DesktopError>>,
) -> Result<T, DispatchError> {
    let child_ticket = child.ticket.clone();
    tokio::pin!(read);
    let outcome = tokio::select! {
        biased;
        _ = parent.ticket.cancelled() => {
            child_ticket.request_cancel();
            read.await
        }
        result = &mut read => result,
    };
    outcome.map_err(|error| DispatchError::from_core(&error))
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
        if let Some(error) = authority
            .connection_parameter_source_failure(&connection.peer_id)
            .await
        {
            return Err(DispatchError::from_core(&error));
        }
        let mut measured = connection_parameters_within_request(
            authority.as_ref(),
            &connection.peer_id,
            &connection.lease,
            &ctl,
        )
        .await?;
        // The receiver existed before the probe. Its newer observations win
        // over a delayed getter; loss requires a live re-read, never zeros.
        let mut opening_events = 0usize;
        let mut opening_values = VecDeque::new();
        loop {
            let next = receiver.try_recv();
            if matches!(&next, Err(broadcast::error::TryRecvError::Empty)) {
                break;
            }
            opening_events += 1;
            if opening_events > MAX_PENDING_EVENTS {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "stream",
                    "tauri.parameters.opening-continuity",
                ));
            }
            match next {
                Ok(event)
                    if event.peer_id == connection.peer_id
                        && event.connection_generation.as_deref()
                            == Some(connection.core_generation.as_str()) =>
                {
                    if let Some(error) = event.error {
                        return Err(DispatchError::from_core(&error));
                    }
                    if event.missed != 0 {
                        opening_values.clear();
                        measured = connection_parameters_within_request(
                            authority.as_ref(),
                            &connection.peer_id,
                            &connection.lease,
                            &ctl,
                        )
                        .await?;
                    } else {
                        opening_values.push_back(ubm_desktop::ObservedConnectionParameters {
                            interval_us: event.interval_us,
                            latency: event.latency,
                            supervision_timeout_us: event.supervision_timeout_us,
                        });
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    opening_values.clear();
                    measured = connection_parameters_within_request(
                        authority.as_ref(),
                        &connection.peer_id,
                        &connection.lease,
                        &ctl,
                    )
                    .await?;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(DispatchError::new(
                        BleErrorCode::StreamClosed,
                        "stream",
                        "tauri.parameters.opening-source",
                    ))
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
            }
        }
        if let Some(first) = opening_values.pop_front() {
            measured = first;
        }
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
        let native_lease = connection.lease.clone();
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
                    received = async {
                        if let Some(measured) = opening_values.pop_front() {
                            return Ok(ubm_desktop::ConnectionParametersEvent {
                                sequence: 0, peer_id: peer_id.clone(),
                                connection_generation: Some(connection_generation.clone()),
                                interval_us: measured.interval_us, latency: measured.latency,
                                supervision_timeout_us: measured.supervision_timeout_us, error: None, missed: 0,
                            });
                        }
                        match receiver.recv().await {
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                                Ok(ubm_desktop::ConnectionParametersEvent {
                                    sequence: 0, peer_id: peer_id.clone(),
                                    connection_generation: Some(connection_generation.clone()),
                                    interval_us: 0, latency: 0, supervision_timeout_us: 0,
                                    error: None, missed,
                                })
                            }
                            answer => answer,
                        }
                    } => {
                        match received {
                            Ok(event)
                                if event.peer_id == peer_id
                                    && event.connection_generation.as_deref()
                                        == Some(connection_generation.as_str()) =>
                            {
                                if let Some(error) = &event.error {
                                    let failure = DispatchError::from_core(error);
                                    let _ = dispatcher.terminal(&task_key, (&lease.0, &lease.1), &stream,
                                        "source-failed", Some(&failure)).await;
                                    break;
                                }
                                let measured = if event.missed != 0 {
                                    match authority.connection_parameters(&peer_id, &native_lease, OpControl::unbounded()).await {
                                        Ok(measured) => measured,
                                        Err(error) => {
                                            let failure = DispatchError::from_core(&error);
                                            let _ = dispatcher.terminal(&task_key, (&lease.0, &lease.1), &stream,
                                                "source-failed", Some(&failure)).await;
                                            break;
                                        }
                                    }
                                } else {
                                    ubm_desktop::boundary::ObservedConnectionParameters {
                                        interval_us: event.interval_us, latency: event.latency,
                                        supervision_timeout_us: event.supervision_timeout_us,
                                    }
                                };
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
                                    interval_us: measured.interval_us,
                                    latency: measured.latency,
                                    supervision_timeout_us: measured.supervision_timeout_us,
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
                                    BleErrorCode::LifecycleInvariantViolation,
                                    "stream",
                                    "tauri.connection-parameters-unreconciled-lag",
                                );
                                let _ = dispatcher
                                    .terminal(
                                        &task_key,
                                        (&lease.0, &lease.1),
                                        &stream,
                                        "source-failed",
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
            if owner.parameter_watches.len() >= MAX_PENDING_EVENTS {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opening_parameter_read_keeps_the_caller_budget_and_cancel() {
        let parent = OpControl::budget_ms(5_000);
        let child = OpControl::new(parent.budget, OpTicket::new());
        assert_eq!(child.budget.deadline(), parent.budget.deadline());
        assert!(child.budget.deadline().is_some());

        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify = std::sync::Arc::clone(&started);
        let child_ticket = child.ticket.clone();
        let read = async move {
            notify.notify_one();
            child_ticket.cancelled().await;
            Err::<(), ubm_desktop::DesktopError>(child_ticket.interruption("connection.parameters"))
        };
        let pending = finish_within_caller_control(&parent, &child, read);
        let cancel = async {
            started.notified().await;
            parent.ticket.request_cancel();
        };
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (result, ()) = tokio::join!(pending, cancel);
            result
        })
        .await
        .expect("caller cancel ends the opening read")
        .expect_err("a cancelled opening read is not a measurement");
        assert_eq!(
            error.identity(),
            (
                "operation.aborted",
                "connection",
                "connection.parameters".to_owned()
            )
        );
        assert!(child.ticket.is_cancel_requested());
        assert!(!parent.ticket.is_settled());
    }
}
