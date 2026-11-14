// vendor/btleplug/src/bluez/peer_source_peripheral.rs
//! Peripheral-facing operations backed by the BlueZ peer source.

use super::*;

// Own both the physical-operation lease and its retained receipt even when
// the runtime discards the spawned future without polling it.
struct MarkerReplyGuard {
    operation: GraphOperationGuard,
    receipt: SourceWriteReceipt,
}
impl Drop for MarkerReplyGuard {
    fn drop(&mut self) {
        self.receipt.settle(Err(crate::PlatformError::new(
            "bluez", "write-reply-abandoned",
            "The write reply was abandoned; the remote effect is unknown",
        )));
    }
}

impl super::super::peripheral::Peripheral {
    /// Capture only the graph currently published by this exact physical owner.
    /// The host must hold the matching central connection admission while calling.
    pub fn capture_reclaim_graph_binding(
        &self,
        peer_id: &str,
        connection_generation: &str,
        expected: &(String, u64, u64),
    ) -> Result<crate::ubm_reclaim_graph_binding::GraphBinding, SourceGroupError> {
        let graph = self.peer.services.lock().map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let state = self.peer.state.lock().map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let matches = graph.accepted_token.as_ref().is_some_and(|token| {
            token.daemon_owner == expected.0 && token.attachment == expected.1 && token.revision == expected.2
        });
        if !state.watching || !state.connected || !state.resolved
            || state.graph_revision != Some(state.revision) || !matches {
            return Err(SourceGroupError::StaleGeneration);
        }
        state.reclaim_binding.capture(peer_id.to_owned(), connection_generation.to_owned(), expected)
            .ok_or(SourceGroupError::StaleGeneration)
    }

    /// Begin on the peer source fence; the reply task outlives the caller.
    pub async fn start_ordered_marker_write(
        &self,
        group: &SourceGroupHandle,
        expected: &(String, u64, u64),
        characteristic: &crate::api::Characteristic,
        request: SourceWriteRequest,
    ) -> Result<SourceWriteReceipt, SourceGroupError> {
        let _wake_scope = self.peer.faults.defer_wakes();
        if request.graph != crate::common::util::marker_graph_identity(expected)
            || Self::peer_route_for_marker(characteristic) != request.route
        {
            return Err(SourceGroupError::TransactionConflict);
        }
        if let Some(receipt) = group.replay_marker_write(&request)? {
            return Ok(receipt);
        }
        let retained_request = request.clone();
        let outcome = (|| {
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
            let graph = self
                .peer
                .services
                .lock()
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
            let mut state = self
                .peer
                .state
                .lock()
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
            let accepted = graph.accepted_token.as_ref();
            if !state.watching
                || !state.connected
                || !state.resolved
                || state.graph_revision != Some(state.revision)
                || accepted.is_none_or(|token| {
                    token.daemon_owner != expected.0
                        || token.attachment != expected.1
                        || token.revision != expected.2
                })
                || state.daemon.as_deref() != Some(expected.0.as_str())
                || request.graph != crate::common::util::marker_graph_identity(expected)
                || Self::peer_route_for_marker(characteristic) != request.route
            {
                return Err(SourceGroupError::StaleGeneration);
            }
            let selected = super::super::peripheral::get_characteristic(
                &graph.services,
                &request.route.service_uuid,
                request.route.service_instance,
                &request.route.characteristic_uuid,
                request.route.characteristic_instance,
            )
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
            let daemon = state
                .daemon
                .as_ref()
                .ok_or(SourceGroupError::SourceStateUnavailable)?;
            let options = bluez_async::WriteOptions {
                write_type: Some(request.write_type.into()),
                ..Default::default()
            };
            let value = request.value.clone();
            if state.reclaim_gate.as_ref().is_some_and(crate::ubm_reclaim_graph_gate::GraphGate::is_busy)
                || state.graph_operations >= 256
            {
                return Err(SourceGroupError::SourceStateUnavailable);
            }
            let start = self
                .peer
                .faults
                .start_source_marker_write(group, request, || {
                    Ok(self.session.start_characteristic_write(
                        daemon,
                        &selected.info.id,
                        value,
                        options,
                    ))
                })?;
            if matches!(&start, SourceWriteStart::Initiated { .. }) {
                state.graph_operations += 1;
            }
            drop(state);
            drop(graph);
            match start {
                SourceWriteStart::Replay(receipt) => Ok(receipt),
                SourceWriteStart::Rejected { receipt, wake }
                | SourceWriteStart::InitiationFailed { receipt, wake } => {
                    for consumer in wake {
                        consumer.wake();
                    }
                    Ok(receipt)
                }
                SourceWriteStart::Initiated {
                    receipt,
                    operation,
                    wake,
                } => {
                    let guard = MarkerReplyGuard {
                        operation: GraphOperationGuard {
                            peer: self.peer.clone(),
                            scope: GraphOperationScope::SingleReplyEffect(Some(expected.0.clone())),
                            settlement: GraphOperationSettlement::Indeterminate,
                        },
                        receipt: receipt.clone(),
                    };
                    runtime.spawn(async move {
                        let mut guard = guard;
                        let result = operation.await.map_err(|error| {
                            crate::PlatformError::bluez_dbus(error.name(), error.message())
                        });
                        let classified = result.as_ref().map(|_| ()).map_err(|error| crate::Error::Platform(error.clone()));
                        guard.operation.settlement = graph_operation_settlement(&guard.operation.scope, &classified);
                        guard.receipt.settle(result);
                    });
                    for consumer in wake {
                        consumer.wake();
                    }
                    Ok(receipt)
                }
            }
        })();
        match outcome {
            Err(SourceGroupError::TransactionConflict) => {
                Err(SourceGroupError::TransactionConflict)
            }
            Err(error) => group.record_marker_not_admitted(retained_request, error),
            Ok(receipt) => Ok(receipt),
        }
    }

    fn peer_route_for_marker(characteristic: &crate::api::Characteristic) -> NotificationRoute {
        PeerSource::route(characteristic)
    }

    /// The generation currently owned by the native notification source.
    pub fn current_notification_source_generation(&self) -> Result<Option<u64>, SourceGroupError> {
        self.peer
            .state
            .lock()
            .map(|state| state.generation)
            .map_err(|_| SourceGroupError::SourceStateUnavailable)
    }

    pub async fn start_notification_source_generation(
        &self,
        generation: u64,
    ) -> Result<(), PeerSourceError> {
        let mut state = self.peer.state.lock().expect("peer source lock poisoned");
        if !state.watching
            || !state.connected
            || !state.resolved
            || state.graph_revision != Some(state.revision)
            || state.generation.is_some()
        {
            return Err(PeerSourceError::Source(SourceGroupError::StaleGeneration));
        }
        self.peer
            .faults
            .start_source_generation(generation)
            .map_err(PeerSourceError::Source)?;
        state.generation = Some(generation);
        Ok(())
    }
    pub fn open_notification_source_group(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<(SourceGroupHandle, SourceGroupConsumer), SourceGroupError> {
        let state = self.peer.state.lock().expect("peer source lock poisoned");
        if state.generation != Some(generation)
            || !state.watching
            || state.graph_revision != Some(state.revision)
        {
            return Err(SourceGroupError::StaleGeneration);
        }
        self.peer
            .faults
            .open_source_group(generation, max_records, max_bytes)
    }
    pub fn register_notification_source_route(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        member: u64,
    ) -> Result<Membership, SourceGroupError> {
        self.register_source_route_inner(group, route, member, None)
    }
    pub fn register_notification_source_route_for_activation(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        member: u64,
        activation: u64,
    ) -> Result<Membership, SourceGroupError> {
        self.register_source_route_inner(group, route, member, Some(activation))
    }
    fn register_source_route_inner(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        member: u64,
        activation: Option<u64>,
    ) -> Result<Membership, SourceGroupError> {
        let _wake_scope = self.peer.faults.defer_wakes();
        let services = self
            .peer
            .services
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let state = self
            .peer
            .state
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if !state.watching || state.graph_revision != Some(state.revision) {
            return Err(SourceGroupError::SourceStateUnavailable);
        }
        super::super::peripheral::get_characteristic(
            &services.services,
            &route.service_uuid,
            route.service_instance,
            &route.characteristic_uuid,
            route.characteristic_instance,
        )
        .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        match activation {
            Some(activation) => self
                .peer
                .faults
                .register_source_route_for_activation(group, route, member, activation),
            None => self.peer.faults.register_source_route(group, route, member),
        }
    }
    pub fn notification_source_barrier(
        &self,
        group: &SourceGroupHandle,
        id: u64,
    ) -> Result<(), SourceGroupError> {
        self.peer.faults.source_group_barrier(group, id)
    }
    pub fn retire_notification_source_route(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        membership: Membership,
    ) -> Result<(), SourceGroupError> {
        self.peer
            .faults
            .retire_source_route(group, route, membership)
    }
    /// Detach after the caller has observed the exact FIFO barrier.
    pub fn detach_notification_source_group_after_barrier(
        &self,
        group: &SourceGroupHandle,
        barrier_id: u64,
    ) -> Result<(), SourceGroupError> {
        self.peer
            .faults
            .detach_source_group_after_barrier(group, barrier_id)
    }
    pub fn detach_notification_source_group(
        &self,
        group: &SourceGroupHandle,
    ) -> Result<(), SourceGroupError> {
        self.peer.faults.detach_source_group(group)
    }
}
