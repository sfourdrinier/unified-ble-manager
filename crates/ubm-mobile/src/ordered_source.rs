// crates/ubm-mobile/src/ordered_source.rs
//! Native ordered-source ownership over the platform radio boundary.
use super::*;
use ubm_core::ordered_notifications::Membership;
use ubm_desktop::boundary::{
    OrderedSourceCleanup, OrderedSourceFailure, OrderedSourceIngress, OrderedSourceLease,
    OrderedSourceRequest, PreparedNotificationActivation,
};

fn problem(operation: &'static str, detail: &'static str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Gatt,
        operation,
    )
    .with_detail(detail)
}

fn pending_drain() -> DesktopError {
    DesktopError::new(
        BleErrorCode::OperationTimedOut,
        BleErrorDomain::Gatt,
        "gatt.ordered.drain",
    )
    .with_detail("ordered close or evidence drain remains pending")
}

struct RecoverAbandonedOpen<'a> {
    radio: &'a ForeignRadio,
    request_id: RequestId,
    collection_id: u64,
    peer_id: String,
    armed: bool,
}

impl Drop for RecoverAbandonedOpen<'_> {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.radio.shared.ordered_recovery)
                .entry(self.request_id)
                .or_default();
            lock(&self.radio.shared.ordered_cleanup_due)
                .insert(self.collection_id, (self.peer_id.clone(), self.request_id));
            self.radio.shared.ordered_open_changed.notify_waiters();
        }
    }
}

impl ForeignRadio {
    pub(super) async fn recover_v2_ordered_source_locked(
        &self,
        peer: &str,
        group_id: u64,
        mapping: crate::ordered_open_slots::GroupMapping,
        entry: RetainedOrderedOpen,
    ) -> Result<ubm_desktop::boundary::OrderedSourceRecovery, DesktopError> {
        use ubm_desktop::boundary::OrderedSourceRecovery;
        let coordinator = self
            .ordered_open_coordinator()
            .ok()
            .flatten()
            .ok_or_else(|| problem("gatt.ordered.recover", "v2 coordinator unavailable"))?;
        let active = coordinator.outstanding().into_iter().find(|active| {
            active.lease.request.local_group == Some(mapping)
                && active.lease.request.request_id == entry.request_id
        });
        if let Some(active) = active {
            let owner = self
                .ordered_admission_owner
                .ok_or_else(|| problem("gatt.ordered.recover", "v2 owner unavailable"))?;
            let identity = active
                .lease
                .wire_identity(owner)
                .map_err(|_| problem("gatt.ordered.recover", "v2 identity unavailable"))?;
            if entry.completion.is_none() {
                let recovery = tokio::time::timeout(
                    ORDERED_OPEN_RECEIPT_TIMEOUT,
                    self.call(|id| {
                        RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Recover {
                            id,
                            identity,
                        })
                    }),
                );
                tokio::pin!(recovery);
                loop {
                    let changed = self.ordered_open_changed().notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if self.retained_ordered_open(group_id).is_some_and(|current| {
                        current.request_id == entry.request_id && current.completion.is_some()
                    }) {
                        break;
                    }
                    tokio::select! {
                        answer = &mut recovery => {
                            match answer {
                                Ok(Ok(_)) => break,
                                Ok(Err(error)) => return Err(error),
                                Err(_) => return Err(problem("gatt.ordered.recover", "v2 recovery reply timed out; ownership retained")),
                            }
                        }
                        () = &mut changed => {}
                    }
                }
            }
            if self
                .retained_ordered_open(group_id)
                .is_some_and(|current| current.completion.is_some())
            {
                self.acknowledge_ordered_open_locked(peer, group_id).await?;
                if matches!(
                    self.retained_ordered_open(group_id)
                        .and_then(|current| current.completion),
                    Some(RadioCompletion::OrderedOpenFailed {
                        ownership: crate::OrderedOpenOwnership::NotCreated,
                        ..
                    })
                ) {
                    coordinator
                        .forget_no_effect_radio_group(mapping, active.lease.identity)
                        .map_err(|_| {
                            problem(
                                "gatt.ordered.recover",
                                "v2 no-effect ownership remains retained",
                            )
                        })?;
                    if !self.release_retained_ordered_open(group_id) {
                        return Err(problem("gatt.ordered.recover", "retained open disappeared"));
                    }
                    self.ordered_host()?
                        .ordered_wakes
                        .remove(mapping.native_group_id);
                    self.forget_v2_attempts(entry.request_id);
                    return Ok(OrderedSourceRecovery::NotStarted);
                }
            }
        }
        Ok(match self.retained_lease(peer, group_id)? {
            Some(lease) => OrderedSourceRecovery::Ready(lease),
            None => OrderedSourceRecovery::Pending,
        })
    }

    pub(super) fn retain_v2_open_completion(
        &self,
        identity: &crate::OrderedOpenIdentity,
        completion: &RadioCompletion,
    ) -> bool {
        use crate::{OrderedOpenCompletionV2, OrderedOpenOutcomeV2, OrderedOpenOwnershipV2};
        let Some(coordinator) = self.ordered_open_coordinator().ok().flatten() else {
            return false;
        };
        let active = coordinator.outstanding().into_iter().find(|entry| {
            entry.lease.request.request_id == identity.original_request_id
                && entry
                    .lease
                    .wire_identity(identity.owner_incarnation)
                    .ok()
                    .as_ref()
                    == Some(identity)
        });
        let RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(result)) = completion
        else {
            return true;
        };
        if result.terminal_for(identity).is_none() {
            return true;
        }
        let legacy = match &result.outcome {
            OrderedOpenOutcomeV2::Opened {
                source_generation,
                members,
            } => RadioCompletion::OrderedOpened {
                generation: source_generation.clone(),
                members: members.clone(),
            },
            OrderedOpenOutcomeV2::Failed {
                original_failure,
                ownership,
            } => RadioCompletion::OrderedOpenFailed {
                failure: original_failure.clone(),
                ownership: match ownership {
                    OrderedOpenOwnershipV2::NotCreated => crate::OrderedOpenOwnership::NotCreated,
                    OrderedOpenOwnershipV2::Created {
                        source_generation,
                        members,
                    } => crate::OrderedOpenOwnership::Created {
                        generation: source_generation.clone(),
                        members: members.clone(),
                    },
                },
            },
            _ => return true,
        };
        if let Some(active) = active {
            if coordinator
                .retain_terminal(&active.lease, active.lease.identity, completion.clone())
                .is_err()
            {
                return false;
            }
            if let Some(mapping) = active.lease.request.local_group {
                let mut retained = lock(&self.shared.retained_ordered_opens);
                if let Some(entry) = retained.get_mut(&mapping.collection_id)
                    && entry.request_id == identity.original_request_id
                {
                    if entry
                        .completion
                        .as_ref()
                        .is_some_and(|existing| existing != &legacy)
                    {
                        return false;
                    }
                    if entry.completion.is_none() {
                        entry.completion = Some(legacy);
                        self.shared.ordered_open_changed.notify_waiters();
                    }
                } else {
                    return false;
                }
            }
            true
        } else {
            // The slot may already be retired while the created source is still owned.
            lock(&self.shared.retained_ordered_opens)
                .values()
                .any(|entry| {
                    entry.request_id == identity.original_request_id
                        && entry.completion.as_ref() == Some(&legacy)
                })
        }
    }

    /// Validate every native-created source before a v2 waiter can observe it.
    pub(super) fn valid_v2_open_payload(
        request: &RadioRequest,
        completion: &RadioCompletion,
    ) -> bool {
        use crate::{
            OrderedOpenCompletionV2, OrderedOpenOutcomeV2, OrderedOpenOwnershipV2,
            OrderedOpenRequestV2,
        };
        let RadioRequest::OrderedOpenV2(request) = request else {
            return true;
        };
        let identity = match request {
            OrderedOpenRequestV2::Open { identity, .. }
            | OrderedOpenRequestV2::Recover { identity, .. } => identity,
            OrderedOpenRequestV2::Acknowledge { .. } => return true,
        };
        let RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(result)) = completion
        else {
            return true;
        };
        let legacy = match &result.outcome {
            OrderedOpenOutcomeV2::Opened {
                source_generation,
                members,
            } => RadioCompletion::OrderedOpened {
                generation: source_generation.clone(),
                members: members.clone(),
            },
            OrderedOpenOutcomeV2::Failed {
                original_failure,
                ownership:
                    OrderedOpenOwnershipV2::Created {
                        source_generation,
                        members,
                    },
            } => RadioCompletion::OrderedOpenFailed {
                failure: original_failure.clone(),
                ownership: crate::OrderedOpenOwnership::Created {
                    generation: source_generation.clone(),
                    members: members.clone(),
                },
            },
            _ => return true,
        };
        let entry = RetainedOrderedOpen {
            request_id: identity.original_request_id,
            peer_id: identity.peer_id.clone(),
            connection_generation: identity.spec.connection_generation.clone(),
            database_generation: identity.spec.database_generation.clone(),
            members: identity.spec.members.clone(),
            completion: Some(legacy),
            acknowledged: false,
        };
        Self::retained_entry_lease(entry, &identity.peer_id, identity.native_group_id).is_ok()
    }

    pub(super) fn ordered_host(&self) -> Result<Arc<HostInner>, DesktopError> {
        lock(&self.shared.connect_generation_hook)
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| problem("gatt.ordered.source", "mobile host is unavailable"))
    }

    pub(super) fn retained_lease(
        &self,
        peer: &str,
        group_id: u64,
    ) -> Result<Option<OrderedSourceLease>, DesktopError> {
        let Some(entry) = self.retained_ordered_open(group_id) else {
            return Ok(None);
        };
        let mut lease = Self::retained_entry_lease(entry, peer, group_id)?;
        if let Some(lease) = &mut lease
            && let Ok(host) = self.ordered_host()
            && let Some(mapping) = host.ordered_group(group_id)
        {
            lease.native_group = mapping.native_group_id;
        }
        Ok(lease)
    }

    pub(super) fn retained_entry_lease(
        entry: RetainedOrderedOpen,
        peer: &str,
        group_id: u64,
    ) -> Result<Option<OrderedSourceLease>, DesktopError> {
        if entry.peer_id != peer {
            return Err(problem(
                "gatt.ordered.recover",
                "peer does not own retained group",
            ));
        }
        let Some(completion) = entry.completion else {
            return Ok(None);
        };
        let (generation, registrations, partial) = match completion {
            RadioCompletion::OrderedOpened {
                generation,
                members,
            } => (generation, members, false),
            RadioCompletion::OrderedOpenFailed {
                ownership:
                    crate::OrderedOpenOwnership::Created {
                        generation,
                        members,
                    },
                ..
            } => (generation, members, true),
            RadioCompletion::OrderedOpenFailed {
                ownership:
                    crate::OrderedOpenOwnership::NotCreated
                    | crate::OrderedOpenOwnership::Indeterminate { .. },
                ..
            } => return Ok(None),
            // Only the typed open result can establish NotCreated. A generic
            // failure reports no native ownership disposition, so keep the
            // retained source pending for recovery or exact cleanup.
            RadioCompletion::Failed(_) => return Ok(None),
            _ => {
                return Err(problem(
                    "gatt.ordered.recover",
                    "native open identity is indeterminate",
                ));
            }
        };
        if matches!(&generation, crate::OrderedSourceGeneration::Opaque(value) if value.is_empty())
        {
            return Err(problem(
                "gatt.ordered.recover",
                "empty native source generation",
            ));
        }
        let expected: HashSet<_> = entry
            .members
            .iter()
            .map(|member| member.member_id)
            .collect();
        let mut ids = HashSet::new();
        let mut memberships = HashSet::new();
        let members: Vec<_> = registrations
            .into_iter()
            .map(|member| {
                (
                    member.member_id,
                    Membership {
                        member: member.source_member,
                        epoch: member.epoch,
                    },
                )
            })
            .collect();
        if (!partial && members.len() != expected.len())
            || members.len() > expected.len()
            || members.iter().any(|(id, membership)| {
                !expected.contains(id) || !ids.insert(*id) || !memberships.insert(*membership)
            })
        {
            return Err(problem(
                "gatt.ordered.recover",
                "native open returned mismatched memberships",
            ));
        }
        Ok(Some(OrderedSourceLease {
            peer_id: peer.to_owned(),
            connection_generation: entry.connection_generation,
            collection_id: group_id,
            source_generation: generation,
            native_group: group_id,
            members,
            ingress: OrderedSourceIngress::CentralRetainedBatch,
        }))
    }

    #[expect(
        clippy::result_large_err,
        reason = "the versioned ordered source contract returns its rich failure by value"
    )]
    pub(super) async fn ordered_prepare_source(
        &self,
        request: OrderedSourceRequest,
    ) -> Result<OrderedSourceLease, OrderedSourceFailure> {
        let group_id = request.collection_id;
        let peer = request.peer_id.clone();
        let failed_without_cleanup = |error| OrderedSourceFailure {
            error,
            cleanup: None,
        };
        if peer.is_empty()
            || request.connection_generation.is_empty()
            || request.database_generation.is_empty()
            || request.members.is_empty()
            || request.max_controls == 0
            || u64::try_from(request.max_controls).map_or(true, |count| {
                count > ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
            })
            || request
                .members
                .iter()
                .any(|member| member.activation.scope.0 != peer)
        {
            return Err(failed_without_cleanup(problem(
                "gatt.ordered.source",
                "invalid source identity or controls",
            )));
        }
        let host = self.ordered_host().map_err(failed_without_cleanup)?;
        let snapshot_identity = request.snapshot_identity.clone().ok_or_else(|| {
            failed_without_cleanup(problem(
                "gatt.ordered.source",
                "native snapshot identity is unavailable",
            ))
        })?;
        let item_capacity = u32::try_from(request.max_items).map_err(|_| {
            failed_without_cleanup(problem("gatt.ordered.source", "item capacity out of range"))
        })?;
        let byte_capacity = u32::try_from(request.max_bytes).map_err(|_| {
            failed_without_cleanup(problem("gatt.ordered.source", "byte capacity out of range"))
        })?;
        let members = request
            .members
            .iter()
            .map(|member| {
                let (
                    peer_id,
                    service_uuid,
                    service_occurrence,
                    characteristic_uuid,
                    characteristic_occurrence,
                ) = &member.activation.scope;
                crate::OrderedMember {
                    member_id: member.member_id,
                    instance: Instance {
                        peer_id: peer_id.clone(),
                        service_uuid: service_uuid.clone(),
                        service_occurrence: *service_occurrence,
                        characteristic_uuid: characteristic_uuid.clone(),
                        characteristic_occurrence: *characteristic_occurrence,
                    },
                }
            })
            .collect();
        if self.ordered_admission_negotiation.is_some_and(|n| {
            n.version == crate::ORDERED_ADMISSION_SLOT_VERSION
                && Some(n.owner) == self.ordered_admission_owner
        }) {
            return self
                .ordered_prepare_v2(
                    request,
                    host,
                    snapshot_identity,
                    members,
                    item_capacity,
                    byte_capacity,
                )
                .await;
        }
        host.ordered_wakes.begin(group_id).map_err(|_| {
            failed_without_cleanup(problem(
                "gatt.ordered.source",
                "ordered wake reservation failed",
            ))
        })?;
        let mut original_request_id = None;
        let result = self
            .call(|id| {
                original_request_id = Some(id);
                RadioRequest::OrderedOpen {
                    id,
                    group_id,
                    connection_generation: request.connection_generation.clone(),
                    database_generation: request.database_generation.clone(),
                    snapshot_identity,
                    members,
                    item_capacity,
                    byte_capacity,
                }
            })
            .await;
        let _lifecycle = self.lock_ordered_open_lifecycle(group_id).await;
        if self
            .retained_ordered_open(group_id)
            .is_some_and(|entry| Some(entry.request_id) != original_request_id)
        {
            return Err(failed_without_cleanup(problem(
                "gatt.ordered.source",
                "open identity was replaced",
            )));
        }
        match result {
            Err(error) => {
                if self.retained_ordered_open(group_id).is_none() {
                    host.ordered_wakes.remove(group_id);
                    return Err(failed_without_cleanup(error));
                }
                return Err(OrderedSourceFailure {
                    error,
                    cleanup: Some(OrderedSourceCleanup::Indeterminate {
                        peer_id: peer,
                        collection_id: group_id,
                    }),
                });
            }
            Ok(RadioCompletion::OrderedOpenFailed { failure, ownership }) => {
                let mut error = failed(
                    &failure,
                    RequestKind::OrderedOpen,
                    self.shared.mobile_platform,
                );
                let cleanup = match ownership {
                    crate::OrderedOpenOwnership::NotCreated => {
                        match self.acknowledge_ordered_open_locked(&peer, group_id).await {
                            Ok(()) if self.release_retained_ordered_open(group_id) => {
                                host.ordered_wakes.remove(group_id);
                                None
                            }
                            Ok(()) => {
                                error = error.with_detail(format!(
                                    "{}; acknowledged open ownership is unavailable",
                                    failure.detail
                                ));
                                Some(OrderedSourceCleanup::Indeterminate {
                                    peer_id: peer.clone(),
                                    collection_id: group_id,
                                })
                            }
                            Err(ack_error) => {
                                error = error.with_detail(format!(
                                    "{}; open receipt acknowledgement failed: {ack_error}",
                                    failure.detail
                                ));
                                Some(OrderedSourceCleanup::Indeterminate {
                                    peer_id: peer.clone(),
                                    collection_id: group_id,
                                })
                            }
                        }
                    }
                    crate::OrderedOpenOwnership::Created { .. } => {
                        // Cleanup retains the lease even if acknowledgement fails.
                        if let Err(ack_error) =
                            self.acknowledge_ordered_open_locked(&peer, group_id).await
                        {
                            error = error.with_detail(format!(
                                "{}; open receipt acknowledgement failed: {ack_error}",
                                failure.detail
                            ));
                        }
                        self.retained_lease(&peer, group_id)
                            .ok()
                            .flatten()
                            .map(OrderedSourceCleanup::Lease)
                            .or(Some(OrderedSourceCleanup::Indeterminate {
                                peer_id: peer.clone(),
                                collection_id: group_id,
                            }))
                    }
                    crate::OrderedOpenOwnership::Indeterminate { .. } => {
                        Some(OrderedSourceCleanup::Indeterminate {
                            peer_id: peer.clone(),
                            collection_id: group_id,
                        })
                    }
                };
                return Err(OrderedSourceFailure { error, cleanup });
            }
            Ok(_) => {}
        }
        match self.retained_lease(&peer, group_id) {
            Ok(Some(lease)) => match self.acknowledge_ordered_open_locked(&peer, group_id).await {
                Ok(()) => Ok(lease),
                Err(error) => Err(OrderedSourceFailure {
                    error,
                    cleanup: Some(OrderedSourceCleanup::Lease(lease)),
                }),
            },
            Ok(None) => Err(OrderedSourceFailure {
                error: problem("gatt.ordered.source", "native open result remains pending"),
                cleanup: Some(OrderedSourceCleanup::Indeterminate {
                    peer_id: peer,
                    collection_id: group_id,
                }),
            }),
            Err(error) => Err(OrderedSourceFailure {
                error,
                cleanup: Some(OrderedSourceCleanup::Indeterminate {
                    peer_id: peer,
                    collection_id: group_id,
                }),
            }),
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "the versioned ordered source contract returns its rich failure by value"
    )]
    async fn ordered_prepare_v2(
        &self,
        request: OrderedSourceRequest,
        host: Arc<HostInner>,
        snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity,
        members: Vec<crate::OrderedMember>,
        item_capacity: u32,
        byte_capacity: u32,
    ) -> Result<OrderedSourceLease, OrderedSourceFailure> {
        let fail = |error| OrderedSourceFailure {
            error,
            cleanup: None,
        };
        let invalid = || {
            problem(
                "gatt.ordered.source",
                "invalid v2 source identity or admission state",
            )
        };
        let mut ids = HashSet::new();
        let mut instances = HashSet::new();
        if snapshot_identity.owner.is_empty()
            || item_capacity == 0
            || byte_capacity == 0
            || u64::from(item_capacity) > ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
            || u64::from(byte_capacity) > ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY
            || u64::try_from(members.len())
                .map_or(true, |n| n > ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY)
            || members
                .iter()
                .any(|m| !ids.insert(m.member_id) || !instances.insert(&m.instance))
        {
            return Err(fail(invalid()));
        }
        let coordinator = self
            .ordered_open_coordinator()
            .ok()
            .flatten()
            .ok_or_else(|| fail(invalid()))?;
        let owner = self
            .ordered_admission_owner
            .ok_or_else(|| fail(invalid()))?;
        let id = Self::next_request_id().map_err(fail)?;
        let mapping = host
            .reserve_ordered_group(request.collection_id)
            .map_err(|_| fail(invalid()))?
            .ok_or_else(|| fail(invalid()))?;
        if host.ordered_wakes.begin(mapping.native_group_id).is_err() {
            let _ = host.forget_unsubmitted_ordered_group(mapping);
            return Err(fail(invalid()));
        }
        let spec = crate::OrderedOpenSpec {
            connection_generation: request.connection_generation.clone(),
            database_generation: request.database_generation.clone(),
            snapshot_identity,
            members: members.clone(),
            item_capacity,
            byte_capacity,
        };
        let slot = match coordinator.acquire(crate::ordered_open_slots::OpenRequest {
            request_id: id,
            group_id: mapping.native_group_id,
            peer: request.peer_id.clone(),
            fingerprint: spec,
            local_group: Some(mapping),
        }) {
            Ok(slot) => slot,
            Err(_) => {
                host.ordered_wakes.remove(mapping.native_group_id);
                let _ = host.forget_unsubmitted_ordered_group(mapping);
                return Err(fail(invalid()));
            }
        };
        {
            let mut retained = lock(&self.shared.retained_ordered_opens);
            if retained.contains_key(&request.collection_id)
                || retained.len() >= RETAINED_ORDERED_OPEN_CAP
            {
                drop(retained);
                let _ = coordinator.release_unsubmitted(&slot);
                host.ordered_wakes.remove(mapping.native_group_id);
                let _ = host.forget_unsubmitted_ordered_group(mapping);
                return Err(fail(invalid()));
            }
            retained.insert(
                request.collection_id,
                RetainedOrderedOpen {
                    request_id: id,
                    peer_id: request.peer_id.clone(),
                    connection_generation: request.connection_generation.clone(),
                    database_generation: request.database_generation.clone(),
                    members,
                    completion: None,
                    acknowledged: false,
                },
            );
        }
        let identity = match coordinator.begin_dispatch_v2(&slot, owner) {
            Ok(identity) => identity,
            Err(_) => {
                lock(&self.shared.retained_ordered_opens).remove(&request.collection_id);
                let _ = coordinator.release_unsubmitted(&slot);
                host.ordered_wakes.remove(mapping.native_group_id);
                let _ = host.forget_unsubmitted_ordered_group(mapping);
                return Err(fail(invalid()));
            }
        };
        let mut recovery = RecoverAbandonedOpen {
            radio: self,
            request_id: id,
            collection_id: request.collection_id,
            peer_id: request.peer_id.clone(),
            armed: true,
        };
        let result = self
            .call_with_id(id, None, false, false, |id| {
                RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity })
            })
            .await;
        let _lifecycle = self
            .lock_ordered_open_lifecycle(request.collection_id)
            .await;
        let cleanup = || {
            Some(OrderedSourceCleanup::Indeterminate {
                peer_id: request.peer_id.clone(),
                collection_id: request.collection_id,
            })
        };
        let result = result.map_err(|error| OrderedSourceFailure {
            error,
            cleanup: cleanup(),
        })?;
        let RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(result)) = result
        else {
            return Err(OrderedSourceFailure {
                error: invalid(),
                cleanup: cleanup(),
            });
        };
        let original_error = match &result.outcome {
            crate::OrderedOpenOutcomeV2::Failed {
                original_failure, ..
            } => Some(failed(
                original_failure,
                RequestKind::OrderedOpenV2,
                self.shared.mobile_platform,
            )),
            _ => None,
        };
        if result
            .terminal_for(
                &slot
                    .wire_identity(owner)
                    .map_err(|_| OrderedSourceFailure {
                        error: invalid(),
                        cleanup: cleanup(),
                    })?,
            )
            .is_none()
        {
            return Err(OrderedSourceFailure {
                error: invalid(),
                cleanup: cleanup(),
            });
        }
        let lease = self
            .retained_lease(&request.peer_id, request.collection_id)
            .map_err(|error| OrderedSourceFailure {
                error,
                cleanup: cleanup(),
            })?;
        let acknowledged = self
            .acknowledge_ordered_open_locked(&request.peer_id, request.collection_id)
            .await;
        if let Some(error) = original_error {
            if lease.is_none()
                && acknowledged.is_ok()
                && coordinator
                    .forget_no_effect_radio_group(mapping, slot.identity)
                    .is_ok()
                && self.release_retained_ordered_open(request.collection_id)
            {
                host.ordered_wakes.remove(mapping.native_group_id);
                self.forget_v2_attempts(id);
                recovery.armed = false;
                return Err(OrderedSourceFailure {
                    error,
                    cleanup: None,
                });
            }
            return Err(OrderedSourceFailure {
                error,
                cleanup: lease.map(OrderedSourceCleanup::Lease).or_else(cleanup),
            });
        }
        match (lease, acknowledged) {
            (Some(lease), Ok(())) => {
                recovery.armed = false;
                Ok(lease)
            }
            (Some(lease), Err(error)) => Err(OrderedSourceFailure {
                error,
                cleanup: Some(OrderedSourceCleanup::Lease(lease)),
            }),
            (_, _) => Err(OrderedSourceFailure {
                error: invalid(),
                cleanup: cleanup(),
            }),
        }
    }

    pub(super) fn validate_ordered_claim(
        &self,
        lease: &OrderedSourceLease,
        member_id: u64,
        membership: Membership,
        activation: &PreparedNotificationActivation,
    ) -> Result<crate::OrderedRegistration, DesktopError> {
        if !lease.members.contains(&(member_id, membership)) {
            return Err(problem("gatt.ordered.claim", "source membership is stale"));
        }
        let retained = self
            .retained_ordered_open(lease.collection_id)
            .ok_or_else(|| problem("gatt.ordered.claim", "source ownership is unavailable"))?;
        let original = self
            .retained_lease(&lease.peer_id, lease.collection_id)?
            .ok_or_else(|| problem("gatt.ordered.claim", "source identity is unresolved"))?;
        if original.connection_generation != lease.connection_generation
            || original.source_generation != lease.source_generation
            || original.native_group != lease.native_group
        {
            return Err(problem("gatt.ordered.claim", "source identity is stale"));
        }
        let expected = retained
            .members
            .iter()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| problem("gatt.ordered.claim", "source member is unavailable"))?;
        let (peer, service, service_occurrence, characteristic, characteristic_occurrence) =
            &activation.scope;
        if expected.instance.peer_id != *peer
            || expected.instance.service_uuid != *service
            || expected.instance.service_occurrence != *service_occurrence
            || expected.instance.characteristic_uuid != *characteristic
            || expected.instance.characteristic_occurrence != *characteristic_occurrence
            || retained.peer_id != lease.peer_id
        {
            return Err(problem(
                "gatt.ordered.claim",
                "physical activation does not match source member",
            ));
        }
        Ok(crate::OrderedRegistration {
            member_id,
            source_member: membership.member,
            epoch: membership.epoch,
        })
    }

    pub(super) fn ordered_activation(
        &self,
        lease: &OrderedSourceLease,
        member_id: u64,
        membership: Membership,
        prepared: &PreparedNotificationActivation,
    ) -> Result<crate::OrderedActivation, DesktopError> {
        let member = self.validate_ordered_claim(lease, member_id, membership, prepared)?;
        let (peer, service, service_occurrence, characteristic, characteristic_occurrence) =
            &prepared.scope;
        Ok(crate::OrderedActivation {
            instance: instance_of(
                peer,
                service,
                *service_occurrence,
                characteristic,
                *characteristic_occurrence,
            ),
            connection_generation: lease.connection_generation.clone(),
            source_generation: lease.source_generation.clone(),
            group_id: lease.native_group,
            member,
            activation_epoch: prepared.activation_epoch,
        })
    }

    pub(super) async fn ordered_retire_member(
        &self,
        lease: &OrderedSourceLease,
        member_id: u64,
        membership: Membership,
    ) -> Result<(), DesktopError> {
        if !lease.members.contains(&(member_id, membership)) {
            return Err(problem(
                "gatt.ordered.member-retire",
                "source membership is stale",
            ));
        }
        let host = self.ordered_host()?;
        let consumer = host.ordered_consumer_for_lease(lease).ok_or_else(|| {
            problem(
                "gatt.ordered.member-retire",
                "ordered consumer is not attached",
            )
        })?;
        let member = crate::OrderedRegistration {
            member_id,
            source_member: membership.member,
            epoch: membership.epoch,
        };
        if consumer.has_consumed_fence(member) {
            return Ok(());
        }
        self.unit(|id| RadioRequest::OrderedRetire {
            id,
            generation: lease.source_generation.clone(),
            group_id: lease.native_group,
            member,
        })
        .await?;
        match consumer.drain_to_empty().await {
            Ok(true) => {}
            Ok(false) => return Err(pending_drain()),
            Err(error) => {
                consumer.mark_failed_on_drain_error(&error);
                return Err(error);
            }
        }
        if !consumer.has_consumed_fence(member) {
            return Err(problem(
                "gatt.ordered.member-retire",
                "member fence was not acknowledged",
            ));
        }
        Ok(())
    }

    pub(super) async fn ordered_reregister_member(
        &self,
        lease: &OrderedSourceLease,
        member_id: u64,
        old: Membership,
    ) -> Result<Membership, DesktopError> {
        if !lease.members.contains(&(member_id, old)) {
            return Err(problem(
                "gatt.ordered.member-register",
                "source membership is stale",
            ));
        }
        let host = self.ordered_host()?;
        let consumer = host.ordered_consumer_for_lease(lease).ok_or_else(|| {
            problem(
                "gatt.ordered.member-register",
                "ordered consumer is not attached",
            )
        })?;
        let member = crate::OrderedRegistration {
            member_id,
            source_member: old.member,
            epoch: old.epoch,
        };
        if !consumer.has_consumed_fence(member) {
            return Err(problem(
                "gatt.ordered.member-register",
                "member fence is not acknowledged",
            ));
        }
        consumer.pause_routing().await;
        let key = OrderedRegisterKey {
            generation: lease.source_generation.clone(),
            group_id: lease.native_group,
            old: member,
        };
        let retained = {
            let entries = lock(&self.shared.retained_ordered_registers);
            entries
                .by_key
                .get(&key)
                .and_then(|id| entries.by_request.get(id))
                .cloned()
        };
        let answer = match retained {
            Some(entry) => entry.completion.ok_or_else(|| {
                problem(
                    "gatt.ordered.member-register",
                    "native registration remains pending",
                )
            })?,
            None => match self
                .call_ordered_register(|id| RadioRequest::OrderedRegister {
                    id,
                    generation: lease.source_generation.clone(),
                    group_id: lease.native_group,
                    member,
                })
                .await
            {
                Ok(answer) => answer,
                Err(error) => {
                    if !lock(&self.shared.retained_ordered_registers)
                        .by_key
                        .keys()
                        .any(|entry| {
                            entry.generation == key.generation && entry.group_id == key.group_id
                        })
                    {
                        consumer.resume_routing();
                    }
                    return Err(error);
                }
            },
        };
        match answer {
            RadioCompletion::OrderedRegistered(new)
                if new.member_id == member_id
                    && new.epoch > old.epoch
                    && new.source_member != old.member =>
            {
                Ok(Membership {
                    member: new.source_member,
                    epoch: new.epoch,
                })
            }
            _ => Err(problem(
                "gatt.ordered.member-register",
                "native registration result is not authoritative",
            )),
        }
    }

    pub(super) async fn ordered_resume_member(
        &self,
        lease: &OrderedSourceLease,
        member_id: u64,
        membership: Membership,
    ) -> Result<(), DesktopError> {
        let host = self.ordered_host()?;
        let consumer = host.ordered_consumer_for_lease(lease).ok_or_else(|| {
            problem(
                "gatt.ordered.member-resume",
                "ordered consumer is not attached",
            )
        })?;
        let mut entries = lock(&self.shared.retained_ordered_registers);
        let resumed_key = (
            lease.source_generation.clone(),
            lease.native_group,
            member_id,
        );
        if entries.resumed.get(&resumed_key) == Some(&membership) {
            return Ok(());
        }
        let found = entries.by_request.iter().find_map(|(id, entry)| {
            if entry.key.generation != lease.source_generation
                || entry.key.group_id != lease.native_group
                || entry.key.old.member_id != member_id
            {
                return None;
            }
            match &entry.completion {
                Some(RadioCompletion::OrderedRegistered(new))
                    if new.member_id == member_id
                        && new.source_member == membership.member
                        && new.epoch == membership.epoch =>
                {
                    Some((*id, entry.key.clone()))
                }
                _ => None,
            }
        });
        let (id, key) = found.ok_or_else(|| {
            problem(
                "gatt.ordered.member-resume",
                "exact registered membership is unavailable",
            )
        })?;
        if !entries.resumed.contains_key(&resumed_key)
            && entries.resumed.len() >= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY as usize
        {
            return Err(problem(
                "gatt.ordered.member-resume",
                "resume receipt capacity exhausted",
            ));
        }
        if !consumer.release_consumed_fence(key.old) {
            return Err(problem(
                "gatt.ordered.member-resume",
                "member fence receipt is unavailable",
            ));
        }
        entries.by_request.remove(&id);
        entries.by_key.remove(&key);
        entries.resumed.insert(resumed_key, membership);
        drop(entries);
        consumer.resume_routing();
        Ok(())
    }

    pub(super) async fn ordered_retire(
        &self,
        lease: &OrderedSourceLease,
        reason: ubm_core::ordered_notifications::EndReason,
    ) -> Result<(), DesktopError> {
        let _barrier_guard = self.lock_ordered_barrier(lease.native_group).await;
        let host = self.ordered_host()?;
        let consumer = host
            .ordered_consumer_for_lease(lease)
            .ok_or_else(|| problem("gatt.ordered.fence", "ordered consumer is not attached"))?;
        consumer.set_close_intent(reason)?;
        let generation = lease.source_generation.clone();
        if !consumer.close_confirmed() {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.unit(|id| RadioRequest::OrderedClose {
                    id,
                    generation,
                    group_id: lease.native_group,
                }),
            )
            .await
            .map_err(|_| pending_drain())??;
            consumer.confirm_close();
        }
        match tokio::time::timeout(std::time::Duration::from_secs(1), consumer.drain_to_empty())
            .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) | Err(_) => return Err(pending_drain()),
            Ok(Err(error)) => {
                consumer.mark_failed_on_drain_error(&error);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) async fn ordered_fence(
        &self,
        lease: &OrderedSourceLease,
    ) -> Result<(), DesktopError> {
        let _guard = self.lock_ordered_barrier(lease.native_group).await;
        let host = self.ordered_host()?;
        let consumer = host
            .ordered_consumer_for_lease(lease)
            .ok_or_else(|| problem("gatt.ordered.barrier", "ordered consumer is not attached"))?;
        let source = ubm_desktop::ordered_notification_groups::SourceKey {
            peer_id: lease.peer_id.clone(),
            generation: lease.source_generation.clone(),
            native_group: lease.native_group,
        };
        self.ordered_barrier_one(source, consumer).await
    }

    pub(super) async fn ordered_release(
        &self,
        cleanup: &OrderedSourceCleanup,
    ) -> Result<ubm_desktop::boundary::OrderedSourceRelease, DesktopError> {
        let group_id = match cleanup {
            OrderedSourceCleanup::Lease(lease) => lease.collection_id,
            OrderedSourceCleanup::Indeterminate { collection_id, .. } => *collection_id,
        };
        let native_group = match cleanup {
            OrderedSourceCleanup::Lease(lease) => lease.native_group,
            OrderedSourceCleanup::Indeterminate { .. } => self
                .ordered_host()?
                .ordered_group(group_id)
                .map_or(group_id, |mapping| mapping.native_group_id),
        };
        let _barrier_guard = self.lock_ordered_barrier(native_group).await;
        let _lifecycle = self.lock_ordered_open_lifecycle(group_id).await;
        let lease = match cleanup {
            OrderedSourceCleanup::Lease(lease) => lease.clone(),
            OrderedSourceCleanup::Indeterminate {
                peer_id,
                collection_id,
            } => self
                .retained_lease(peer_id, *collection_id)?
                .ok_or_else(|| {
                    problem(
                        "gatt.ordered.source-release",
                        "native open remains indeterminate",
                    )
                })?,
        };
        {
            let retained = lock(&self.shared.retained_ordered_enables);
            if retained
                .by_activation
                .keys()
                .chain(retained.reserved.iter())
                .any(|activation| {
                    activation.group_id == lease.native_group
                        && activation.source_generation == lease.source_generation
                })
            {
                return Ok(ubm_desktop::boundary::OrderedSourceRelease::Pending);
            }
        }
        {
            let retained = lock(&self.shared.retained_ordered_registers);
            if retained.by_key.keys().any(|key| {
                key.group_id == lease.native_group && key.generation == lease.source_generation
            }) {
                return Ok(ubm_desktop::boundary::OrderedSourceRelease::Pending);
            }
        }
        let current = self
            .retained_lease(&lease.peer_id, lease.collection_id)?
            .ok_or_else(|| {
                problem(
                    "gatt.ordered.source-release",
                    "source ownership is unavailable",
                )
            })?;
        if current.native_group != lease.native_group
            || current.connection_generation != lease.connection_generation
            || current.source_generation != lease.source_generation
        {
            return Err(problem(
                "gatt.ordered.source-release",
                "source ownership is stale",
            ));
        }
        self.acknowledge_ordered_open_locked(&lease.peer_id, lease.collection_id)
            .await?;
        let host = self.ordered_host()?;
        if let Some(consumer) = host.ordered_consumer_for_lease(&lease)
            && consumer.unreported_failure().is_some()
        {
            consumer.retry_failure_report()?;
        }
        let generation = lease.source_generation.clone();
        let removal_pending = lock(&self.shared.ordered_removal_pending)
            .get(&lease.native_group)
            .cloned();
        if let Some(previous) = removal_pending {
            if previous != generation {
                return Err(problem(
                    "gatt.ordered.source-release",
                    "removal receipt generation mismatch",
                ));
            }
        } else {
            self.unit(|id| RadioRequest::OrderedRemove {
                id,
                generation: generation.clone(),
                group_id: lease.native_group,
            })
            .await?;
            lock(&self.shared.ordered_removal_pending)
                .insert(lease.native_group, generation.clone());
        }
        self.unit(|id| RadioRequest::OrderedForgetRemovalReceipt {
            id,
            generation,
            group_id: lease.native_group,
        })
        .await?;
        lock(&self.shared.ordered_removal_pending).remove(&lease.native_group);
        lock(&self.shared.retained_ordered_enables)
            .abandoned
            .retain(|activation| {
                activation.group_id != lease.native_group
                    || activation.source_generation != lease.source_generation
            });
        lock(&self.shared.retained_ordered_registers)
            .resumed
            .retain(|(generation, group, _), _| {
                *group != lease.native_group || *generation != lease.source_generation
            });
        lock(&self.shared.ordered_pending_barriers).remove(
            &ubm_desktop::ordered_notification_groups::SourceKey {
                peer_id: lease.peer_id.clone(),
                generation: lease.source_generation.clone(),
                native_group: lease.native_group,
            },
        );
        if let Some(mapping) = host.ordered_group(lease.collection_id) {
            let coordinator = self
                .ordered_open_coordinator()
                .ok()
                .flatten()
                .ok_or_else(|| {
                    problem("gatt.ordered.source-release", "v2 coordinator unavailable")
                })?;
            coordinator
                .forget_removed_group(mapping, lease.native_group)
                .map_err(|_| {
                    problem(
                        "gatt.ordered.source-release",
                        "v2 group retirement unavailable",
                    )
                })?;
            if let Some(entry) = self.retained_ordered_open(lease.collection_id) {
                self.forget_v2_attempts(entry.request_id);
            }
        }
        host.retire_ordered_source(&lease);
        if !self.release_retained_ordered_open(lease.collection_id) {
            return Err(problem(
                "gatt.ordered.source-release",
                "retained open ownership is unavailable",
            ));
        }
        Ok(ubm_desktop::boundary::OrderedSourceRelease::Released)
    }

    pub(super) async fn ordered_flush_peer(&self, peer_id: &str) -> Result<(), DesktopError> {
        let host = self.ordered_host()?;
        for (source, consumer) in host.ordered_consumers_for_peer(peer_id) {
            let _guard = self.lock_ordered_barrier(source.native_group).await;
            self.ordered_barrier_one(source, consumer).await?;
        }
        Ok(())
    }

    async fn ordered_barrier_one(
        &self,
        source: ubm_desktop::ordered_notification_groups::SourceKey,
        consumer: Arc<crate::host::ordered_batch::OrderedBatchConsumer>,
    ) -> Result<(), DesktopError> {
        if consumer.failed() {
            return Err(problem("gatt.ordered.barrier", "source consumer failed"));
        }
        if consumer.terminal_acked() {
            if lock(&self.shared.ordered_pending_barriers).contains_key(&source) {
                return Err(problem(
                    "gatt.ordered.barrier",
                    "source ended before pending barrier",
                ));
            }
            return Ok(());
        }
        let pending = lock(&self.shared.ordered_pending_barriers)
            .get(&source)
            .copied();
        let barrier_id = if let Some(id) = pending {
            id
        } else {
            let id = self
                .shared
                .next_ordered_barrier
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| problem("gatt.ordered.barrier", "barrier identifiers exhausted"))?;
            lock(&self.shared.ordered_pending_barriers).insert(source.clone(), id);
            id
        };
        // The native owner deduplicates this barrier identity. Keep it in
        // ordered_pending_barriers when the reply is lost so the next attempt
        // cannot append a second fence or advance beyond an uncertain one.
        let timeout_error = || {
            DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Gatt,
                "gatt.ordered.barrier",
            )
            .with_detail("native barrier reply remains pending; retry retains its identity")
        };
        let mut replied = false;
        for _ in 0..2 {
            match tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.unit(|id| RadioRequest::OrderedBarrier {
                    id,
                    generation: source.generation.clone(),
                    group_id: source.native_group,
                    barrier_id,
                }),
            )
            .await
            {
                Ok(answer) => {
                    answer?;
                    replied = true;
                    break;
                }
                Err(_) => continue,
            }
        }
        if !replied {
            // An ACKed record is direct evidence of this exact native barrier,
            // even if both command replies were lost. Keep the receipt and
            // pending identity on timeout or a failed drain for a later retry.
            tokio::time::timeout(std::time::Duration::from_secs(1), consumer.drain_to_empty())
                .await
                .map_err(|_| timeout_error())??;
            if consumer.take_consumed_barrier(barrier_id).is_some() {
                lock(&self.shared.ordered_pending_barriers).remove(&source);
                return Ok(());
            }
            return Err(timeout_error());
        }
        match consumer.drain_to_empty().await {
            Ok(true) => {}
            Ok(false) => return Err(timeout_error()),
            Err(error) => {
                consumer.mark_failed_on_drain_error(&error);
                return Err(error);
            }
        }
        if consumer.take_consumed_barrier(barrier_id).is_none() {
            return Err(problem(
                "gatt.ordered.barrier",
                if consumer.ended() {
                    "source ended before barrier"
                } else {
                    "native barrier was not acknowledged"
                },
            ));
        }
        lock(&self.shared.ordered_pending_barriers).remove(&source);
        Ok(())
    }
}
