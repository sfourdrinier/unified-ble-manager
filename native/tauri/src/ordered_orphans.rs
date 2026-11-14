// native/tauri/src/ordered_orphans.rs
//! Sealed, payload-free receipts for revoked ordered groups.

use crate::IpcValue;

impl super::ordered_groups::OrphanProgress {
    fn retain_observation(
        &mut self,
        record: &ubm_core::ordered_notifications::Record,
    ) -> Result<(), super::DispatchError> {
        use ubm_core::ordered_notifications::{Poll, Record};
        let encoded = ubm_desktop::ordered_wire::encode_versioned(Poll::Record(record.clone()), 2)
            .ok_or_else(|| invariant("tauri.ordered.orphan-observation"))?;
        let controls = self
            .controls
            .checked_add(1)
            .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
        match record {
            Record::FailureObservation { tail_uncertain, .. } => {
                self.uncertain |= tail_uncertain;
                self.failure_observations.push(encoded);
            }
            Record::FailureObservationsOmitted { .. } => {
                self.failure_observations_omitted.push(encoded)
            }
            _ => return Err(invariant("tauri.ordered.orphan-observation")),
        }
        self.controls = controls;
        Ok(())
    }
}

impl super::BtleplugDispatcher {
    /// Inspect pending revoked groups without exposing notification payloads.
    pub async fn inspect_revoked_ordered_debt(
        &self,
        caller: &crate::AuthenticatedCaller,
    ) -> Vec<IpcValue> {
        let key = super::caller_key(caller);
        if !self.is_revoked(&key) {
            return Vec::new();
        }
        let groups = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .map(|owner| {
                owner
                    .ordered_groups
                    .iter()
                    .map(|(handle, group)| (handle.clone(), group.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut result = Vec::new();
        for (handle, group) in groups {
            let group = group.lock().await;
            result.push(super::object([
                ("handle", super::string(handle)),
                (
                    "state",
                    super::string(if group.orphan.id.is_none() {
                        "receipt-capacity"
                    } else {
                        "source-or-cleanup"
                    }),
                ),
                (
                    "error",
                    group.orphan.error.clone().unwrap_or(IpcValue::Null),
                ),
            ]));
        }
        result
    }

    /// Retry sealed accounting and native cleanup for a revoked host identity.
    pub async fn retry_revoked_ordered_cleanup(
        &self,
        caller: &crate::AuthenticatedCaller,
    ) -> IpcValue {
        let key = super::caller_key(caller);
        let revision = self
            .revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .get(&key)
            .copied();
        let Some(revision) = revision else {
            return super::released();
        };
        self.settle_revoked_ordered(&key).await;
        let cleanup = self.release(&key).await;
        if super::is_released(&cleanup) {
            let mut revoked = self
                .revoked_callers
                .lock()
                .expect("revocation mutex poisoned");
            if revoked.get(&key).copied() == Some(revision) {
                revoked.remove(&key);
            }
        }
        cleanup
    }

    /// Inspect sealed, payload-free receipts retained for this authenticated host identity.
    pub async fn inspect_ordered_orphans(
        &self,
        caller: &crate::AuthenticatedCaller,
    ) -> Vec<IpcValue> {
        let key = super::caller_key(caller);
        self.inner
            .lock()
            .await
            .ordered_orphans
            .iter()
            .filter(|slot| slot.owner == key)
            .filter_map(|slot| slot.receipt.clone())
            .collect()
    }

    /// Acknowledge one sealed receipt after the trusted host has retained it.
    pub async fn acknowledge_ordered_orphan(&self, caller: &crate::AuthenticatedCaller, id: &str) {
        let key = super::caller_key(caller);
        self.inner
            .lock()
            .await
            .ordered_orphans
            .retain(|slot| !(slot.owner == key && slot.id == id && slot.receipt.is_some()));
    }

    pub(super) async fn ordered_orphan_receipts(
        &self,
        caller: &crate::AuthenticatedCaller,
    ) -> Result<IpcValue, super::DispatchError> {
        let key = super::caller_key(caller);
        let state = self.inner.lock().await;
        let receipts = state
            .ordered_orphans
            .iter()
            .filter(|slot| slot.owner == key)
            .filter_map(|slot| {
                let IpcValue::Object(fields) = slot.receipt.as_ref()? else {
                    return None;
                };
                let keys = [
                    "format",
                    "receiptId",
                    "values",
                    "bytes",
                    "controls",
                    "uncertainTail",
                ];
                Some(IpcValue::Object(
                    keys.into_iter()
                        .filter_map(|key| {
                            fields
                                .get(key)
                                .cloned()
                                .map(|value| (key.to_owned(), value))
                        })
                        .collect(),
                ))
            })
            .collect();
        Ok(super::object([
            ("receipts", IpcValue::Array(receipts)),
            ("truncated", IpcValue::Bool(false)),
        ]))
    }

    pub(super) async fn ordered_orphan_page(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let invalid = || {
            super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::ArgumentInvalid,
                "cleanup",
                "tauri.ordered.orphan-page",
            )
        };
        let id = super::required_string(&payload, "receiptId", "tauri.ordered.orphan-page")?;
        let offset = match payload.get("offset") {
            Some(IpcValue::Number(value)) => value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
        let limit = match payload.get("limit") {
            None => 2048,
            Some(IpcValue::Number(value)) => value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0 && *n <= 4096)
                .ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
        let key = super::caller_key(caller);
        let mut state = self.inner.lock().await;
        let slot = state
            .ordered_orphans
            .iter_mut()
            .find(|slot| slot.owner == key && slot.id == id && slot.receipt.is_some())
            .ok_or_else(|| {
                super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::OwnershipDenied,
                    "cleanup",
                    "tauri.ordered.orphan-page",
                )
            })?;
        if offset == 0 && slot.page_start != 0 {
            slot.page_start = 0;
            slot.page_end = None;
            slot.page_done = false;
        }
        if let Some(end) = slot.page_end {
            if offset != slot.page_start && !(offset == end && !slot.page_done) {
                return Err(invalid());
            }
            if offset == end && offset != slot.page_start {
                slot.page_start = offset;
                slot.page_end = None;
            }
        } else if offset != slot.page_start {
            return Err(invalid());
        }
        let text = slot
            .receipt
            .clone()
            .expect("ready receipt")
            .into_wire()
            .to_string();
        let chunk: String = text.chars().skip(slot.page_start).take(limit).collect();
        let next = slot
            .page_start
            .checked_add(chunk.chars().count())
            .ok_or_else(invalid)?;
        if let Some(end) = slot.page_end {
            if next != end {
                return Err(invalid());
            }
        } else {
            slot.page_end = Some(next);
            slot.page_done = chunk.is_empty();
        }
        let next = i64::try_from(next).map_err(|_| invalid())?;
        Ok(super::object([
            ("chunk", super::string(chunk)),
            ("nextOffset", super::number(next)),
            ("done", IpcValue::Bool(slot.page_done)),
        ]))
    }

    pub(super) async fn ack_ordered_orphan(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let id = super::required_string(&payload, "receiptId", "tauri.ordered.orphan-ack")?;
        let key = super::caller_key(caller);
        let mut state = self.inner.lock().await;
        if let Some(index) = state
            .ordered_orphans
            .iter()
            .position(|slot| slot.owner == key && slot.id == id)
        {
            if !state.ordered_orphans[index].page_done {
                return Err(super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                    "cleanup",
                    "tauri.ordered.orphan-unread",
                ));
            }
            state.ordered_orphans.remove(index);
        }
        Ok(super::object([("state", super::string("forgotten"))]))
    }

    pub(super) async fn settle_revoked_ordered(&self, key: &str) {
        let groups = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .map(|owner| {
                owner
                    .ordered_groups
                    .iter()
                    .map(|(handle, group)| (handle.clone(), group.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (handle, group) in groups {
            // A pending or failed step leaves the source and reserved receipt
            // slot owned; the next host recovery/bootstrap retries it.
            if let Err(error) = self.settle_one_revoked(key, &handle, group.clone()).await {
                group.lock().await.orphan.error = Some(error.normalized_error());
            }
        }
    }

    async fn settle_one_revoked(
        &self,
        key: &str,
        handle: &str,
        group_arc: std::sync::Arc<tokio::sync::Mutex<super::ordered_groups::OrderedGroup>>,
    ) -> Result<(), super::DispatchError> {
        use ubm_core::ordered_notifications::Poll;
        use ubm_desktop::central::{OrderedCleanupOutcome, OrderedDiscardEvidence};
        let authority = self.ensure_authority().await?;
        let mut group = group_arc.lock().await;
        // A completed forget means the caller already acknowledged the fully
        // drained receipt. A lost forget response still leaves the exact
        // authenticated mapping, so retry only that idempotent native step.
        if group.core_acknowledged {
            if !group.core_forgotten {
                authority
                    .forget_ordered_batch(&group.peer_id, group.collection_id)
                    .await
                    .map_err(|error| super::DispatchError::from_core(&error))?;
                group.core_forgotten = true;
            }
            let mut state = self.inner.lock().await;
            if let Some(owner) = state.callers.get_mut(key) {
                if owner
                    .ordered_groups
                    .get(handle)
                    .is_some_and(|stored| std::sync::Arc::ptr_eq(stored, &group_arc))
                {
                    owner.ordered_groups.remove(handle);
                }
            }
            return Ok(());
        }
        if group.orphan.id.is_none() {
            let mut state = self.inner.lock().await;
            if state.ordered_orphans.len() >= super::MAX_PENDING_EVENTS {
                return Ok(());
            }
            let id = self.internal_id("ordered-orphan");
            state
                .ordered_orphans
                .push(super::ordered_groups::OrphanSlot {
                    owner: key.to_owned(),
                    id: id.clone(),
                    receipt: None,
                    page_start: 0,
                    page_end: None,
                    page_done: false,
                });
            group.orphan.id = Some(id);
        }
        if !group.orphan.core_acknowledged {
            let marker_owner = authority
                .group_marker_reservation(&group.peer_id, group.collection_id)
                .await
                .map_err(|e| super::DispatchError::from_core(&e))?;
            if group.orphan.marker_write.is_none() {
                if let Some((marker_handle, _)) = marker_owner
                    .active()
                    .await
                    .map_err(|e| super::DispatchError::from_core(&e))?
                {
                    let status = marker_owner
                        .cancel(marker_handle)
                        .await
                        .map_err(|e| super::DispatchError::from_core(&e))?;
                    use ubm_desktop::boundary::{
                        OrderedMarkerWritePhase, OrderedMarkerWriteStatus,
                    };
                    let settled = matches!(
                        &status,
                        OrderedMarkerWriteStatus::NotAdmitted(_)
                            | OrderedMarkerWriteStatus::Rejected(_)
                            | OrderedMarkerWriteStatus::Admitted {
                                write: OrderedMarkerWritePhase::NotStarted(_)
                                    | OrderedMarkerWritePhase::Settled(_),
                                ..
                            }
                    );
                    if !settled {
                        return Ok(());
                    }
                    group.orphan.marker_write = Some((
                        marker_handle,
                        ubm_desktop::ordered_wire::encode_marker_write_versioned(&status, 2)
                            .ok_or_else(|| invariant("tauri.ordered.orphan-marker"))?,
                    ));
                }
            }
            if let Some(id) = group.orphan.pending_receipt_ack {
                authority
                    .acknowledge_ordered_discard(&group.peer_id, group.collection_id, id)
                    .await
                    .map_err(|e| super::DispatchError::from_core(&e))?;
                group.orphan.pending_receipt_ack = None;
            }
            if group.orphan.terminal.is_none() && !group.orphan.source_closed {
                let outcome = authority
                    .close_ordered_batch(
                        &group.peer_id,
                        group.collection_id,
                        ubm_desktop::OpControl::unbounded(),
                    )
                    .await
                    .map_err(|e| super::DispatchError::from_core(&e))?;
                loop {
                    let delivery = authority
                        .poll_ordered_delivery(&group.peer_id, group.collection_id)
                        .await
                        .map_err(|e| super::DispatchError::from_core(&e))?;
                    let Some(id) = delivery.delivery_id else {
                        if matches!(delivery.poll, Poll::Closed)
                            && matches!(outcome, OrderedCleanupOutcome::Released)
                        {
                            group.orphan.uncertain = true;
                            group.orphan.source_closed = true;
                            break;
                        }
                        return Ok(());
                    };
                    if group.pending_native_id == Some(id) {
                        let raw = group
                            .delivery
                            .retained()
                            .map(|(_, item)| item.clone().into_wire())
                            .ok_or_else(|| invariant("tauri.ordered.orphan-retained"))?;
                        let legacy = ubm_desktop::ordered_wire::encode(delivery.poll.clone());
                        let current =
                            ubm_desktop::ordered_wire::encode_versioned(delivery.poll.clone(), 2)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-evidence"))?;
                        if raw != legacy && raw != current {
                            return Err(invariant("tauri.ordered.orphan-raw-evidence"));
                        }
                    }
                    let receipt = authority
                        .discard_ordered_delivery(&group.peer_id, group.collection_id, id)
                        .await
                        .map_err(|e| super::DispatchError::from_core(&e))?;
                    match &receipt.evidence {
                        OrderedDiscardEvidence::Value { bytes, .. } => {
                            let values = group
                                .orphan
                                .values
                                .checked_add(1)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
                            let bytes = group
                                .orphan
                                .bytes
                                .checked_add(*bytes)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
                            group.orphan.values = values;
                            group.orphan.bytes = bytes;
                            group.orphan.trusted_values.push(
                                ubm_desktop::ordered_wire::encode_discard_receipt(&receipt)
                                    ["evidence"]
                                    .clone(),
                            );
                        }
                        OrderedDiscardEvidence::Loss(record) => {
                            group.orphan.controls = group
                                .orphan
                                .controls
                                .checked_add(1)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
                            group.orphan.losses.push(ubm_desktop::ordered_wire::encode(
                                Poll::Record(record.clone()),
                            ));
                        }
                        OrderedDiscardEvidence::Observation(record) => {
                            group.orphan.retain_observation(record)?;
                        }
                        OrderedDiscardEvidence::Marker { marker_id, ordinal } => {
                            group.orphan.controls = group
                                .orphan
                                .controls
                                .checked_add(1)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
                            group.orphan.markers.push(serde_json::json!({"kind":"marker", "markerId":marker_id.to_string(), "ordinal":ordinal.to_string()}));
                        }
                        OrderedDiscardEvidence::Terminal(terminal) => {
                            let encoded = ubm_desktop::ordered_wire::encode_versioned(
                                Poll::Terminal(terminal.clone()),
                                2,
                            )
                            .ok_or_else(|| invariant("tauri.ordered.orphan-terminal"))?;
                            group.orphan.controls = group
                                .orphan
                                .controls
                                .checked_add(1)
                                .ok_or_else(|| invariant("tauri.ordered.orphan-count"))?;
                            group.orphan.uncertain |= terminal.tail_uncertain;
                            group.orphan.terminal = Some(encoded);
                        }
                    }
                    if group.pending_native_id == Some(id) {
                        group.orphan.uncertain = true;
                        group.delivery.take_for_revocation();
                        group.pending_native_id = None;
                    }
                    group.orphan.pending_receipt_ack = Some(receipt.receipt_id);
                    authority
                        .acknowledge_ordered_discard(
                            &group.peer_id,
                            group.collection_id,
                            receipt.receipt_id,
                        )
                        .await
                        .map_err(|e| super::DispatchError::from_core(&e))?;
                    group.orphan.pending_receipt_ack = None;
                    if group.orphan.terminal.is_some() {
                        break;
                    }
                }
            }
            if let Some((marker_handle, _)) = &group.orphan.marker_write {
                marker_owner
                    .forget(*marker_handle)
                    .await
                    .map_err(|e| super::DispatchError::from_core(&e))?;
            }
            let outcome = authority
                .close_ordered_batch(
                    &group.peer_id,
                    group.collection_id,
                    ubm_desktop::OpControl::unbounded(),
                )
                .await
                .map_err(|e| super::DispatchError::from_core(&e))?;
            if !matches!(outcome, OrderedCleanupOutcome::Released) {
                return Ok(());
            }
            let status = super::ordered_groups::native_status(authority.as_ref(), &group).await?;
            group.orphan.final_receipt = Some(super::ordered_groups::json_value(
                serde_json::json!({
                    "format":"native", "receiptVersion":2, "receiptId":group.orphan.id, "values":group.orphan.values.to_string(), "bytes":group.orphan.bytes.to_string(), "controls":group.orphan.controls.to_string(), "uncertainTail":group.orphan.uncertain,
                    "losses":group.orphan.losses, "terminal":group.orphan.terminal, "failureObservations":group.orphan.failure_observations, "failureObservationsOmitted":group.orphan.failure_observations_omitted, "trustedValues":group.orphan.trusted_values, "markers":group.orphan.markers, "markerWrites":group.orphan.marker_write.iter().map(|(handle,status)| serde_json::json!({"markerHandle":handle.to_string(),"statusFormat":"native","status":status})).collect::<Vec<_>>(), "nativeStatus":status,
                    "memberIds":group.member_ids, "collectionId":group.collection_id.to_string(), "connectionGeneration":group.public_connection, "databaseGeneration":group.public_database
                }),
            ));
            authority
                .acknowledge_ordered_batch(&group.peer_id, group.collection_id)
                .await
                .map_err(|e| super::DispatchError::from_core(&e))?;
            group.orphan.core_acknowledged = true;
        }
        // Keep the receipt and exact caller-owned identity until native forget
        // succeeds; the core accepts a retry after a lost forget response.
        authority
            .forget_ordered_batch(&group.peer_id, group.collection_id)
            .await
            .map_err(|error| super::DispatchError::from_core(&error))?;
        let id = group.orphan.id.clone().expect("reserved orphan slot");
        let receipt = group
            .orphan
            .final_receipt
            .clone()
            .expect("receipt before core acknowledgement");
        let mut state = self.inner.lock().await;
        let slot = state
            .ordered_orphans
            .iter_mut()
            .find(|slot| slot.owner == key && slot.id == id)
            .ok_or_else(|| invariant("tauri.ordered.orphan-slot"))?;
        slot.receipt = Some(receipt);
        if let Some(owner) = state.callers.get_mut(key) {
            if owner
                .ordered_groups
                .get(handle)
                .is_some_and(|stored| std::sync::Arc::ptr_eq(stored, &group_arc))
            {
                owner.ordered_groups.remove(handle);
            }
        }
        Ok(())
    }
}

fn invariant(operation: &str) -> super::DispatchError {
    super::DispatchError::new(
        ubm_core::contracts::BleErrorCode::LifecycleInvariantViolation,
        "cleanup",
        operation,
    )
}

#[cfg(test)]
mod revision_observation_tests {
    use super::super::ordered_groups::OrphanProgress;
    use ubm_core::ordered_notifications::Record;

    #[test]
    fn discarded_observations_are_retained_separately_before_ack() {
        let mut orphan = OrphanProgress::default();
        orphan
            .retain_observation(&Record::FailureObservation {
                failure: None,
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: true,
            })
            .unwrap();
        orphan
            .retain_observation(&Record::FailureObservationsOmitted { count: None })
            .unwrap();
        assert_eq!(orphan.controls, 2);
        assert_eq!(orphan.failure_observations[0]["tailUncertain"], true);
        assert!(orphan.failure_observations_omitted[0]["count"].is_null());
    }
}
