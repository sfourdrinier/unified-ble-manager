// native/tauri/src/ordered_reclaims.rs
//! Authenticated process-owned recorder handoff and exact native reclaim receipts.
use super::{BtleplugDispatcher, DispatchError};
use crate::desktop_core::ReclaimReservation;
use crate::{AuthenticatedCaller, IpcValue};
use std::{collections::HashMap, sync::Arc};
use ubm_core::contracts::BleErrorCode;
use ubm_desktop::reclaim_reservation::ReclaimReservationStatus as Status;

#[derive(Clone)]
struct Slot {
    owner: String,
    session: String,
    generation: String,
    database: String,
    connection: String,
    start_handle: String,
    recording_id: String,
    capability: String,
    native_id: u64,
    native_lease: String,
    caller_key: String,
    reservation: Arc<dyn ReclaimReservation>,
    valid: bool,
    committed: bool,
    settled: bool,
    retired: bool,
    acknowledged: bool,
    forgotten: bool,
}
#[derive(Default)]
pub(super) struct ReclaimState {
    slots: HashMap<(String, String), Slot>,
    mappings: HashMap<(String, String), Slot>,
}
impl ReclaimState {
    pub(super) fn invalidate(
        &mut self,
        owner: &str,
        database: Option<&str>,
        connection: Option<&str>,
    ) {
        for slot in self.slots.values_mut().chain(self.mappings.values_mut()) {
            if slot.owner == owner
                && database.is_none_or(|d| d == slot.database)
                && connection.is_none_or(|c| c == slot.connection)
            {
                slot.valid = false;
            }
        }
    }
}
fn denied() -> DispatchError {
    DispatchError::new(
        BleErrorCode::OwnershipDenied,
        "gatt",
        "tauri.ordered-recording.reclaim",
    )
}
fn malformed() -> DispatchError {
    DispatchError::new(
        BleErrorCode::ProtocolMalformed,
        "ipc",
        "tauri.ordered-recording.reclaim",
    )
}
fn receipt(status: Status) -> IpcValue {
    let (state, committed, error) = match status {
        Status::Reserved => ("reserved", false, None),
        Status::Pending => ("pending", false, None),
        Status::Ready => ("ready", false, None),
        Status::Committed => ("committed", true, None),
        Status::Releasing { mapping_committed } => ("releasing", mapping_committed, None),
        Status::Released { mapping_committed } => ("released", mapping_committed, None),
        Status::Retired { mapping_committed } => ("retired", mapping_committed, None),
        Status::ReleaseFailed {
            mapping_committed,
            error,
        } => ("release-failed", mapping_committed, Some(error)),
        Status::Failed(error) => ("failed", false, Some(error)),
    };
    super::object([
        ("state", super::string(state)),
        ("mappingCommitted", IpcValue::Bool(committed)),
        (
            "error",
            error
                .map(|e| DispatchError::from_core(&e).normalized_error())
                .unwrap_or(IpcValue::Null),
        ),
    ])
}
impl BtleplugDispatcher {
    pub(super) fn invalidate_reclaims(
        &self,
        owner: &str,
        database: Option<&str>,
        connection: Option<&str>,
    ) {
        self.reclaim_state
            .lock()
            .expect("reclaim state poisoned")
            .invalidate(owner, database, connection);
    }
    pub(super) fn has_reclaim_debt(
        &self,
        owner: &str,
        database: Option<&str>,
        connection: Option<&str>,
    ) -> bool {
        self.reclaim_state
            .lock()
            .expect("reclaim state poisoned")
            .slots
            .values()
            .any(|slot| {
                slot.owner == owner
                    && !slot.settled
                    && database.is_none_or(|d| slot.database == d)
                    && connection.is_none_or(|c| slot.connection == c)
            })
    }
    pub(super) async fn settle_revoked_reclaims(&self, owner: &str) {
        let _gate = self.reclaim_admission.lock().await;
        let slots: Vec<_> = self
            .reclaim_state
            .lock()
            .expect("reclaim state poisoned")
            .slots
            .iter()
            .filter(|((key, _), slot)| key == owner && !slot.valid && !slot.settled)
            .map(|(key, slot)| (key.clone(), slot.clone()))
            .collect();
        for (key, slot) in slots {
            let status = slot.reservation.status(
                slot.native_id,
                &slot.capability,
                &slot.native_lease,
                &slot.caller_key,
            );
            let action = match status {
                Ok(Status::Reserved | Status::Pending | Status::Ready) => Some("cancel"),
                Ok(Status::Committed | Status::Releasing { .. } | Status::ReleaseFailed { .. }) => {
                    Some("release")
                }
                _ => None,
            };
            if let Some(action) = action {
                if slot
                    .reservation
                    .action(
                        action,
                        slot.native_id,
                        &slot.capability,
                        &slot.native_lease,
                        &slot.caller_key,
                        Box::new(|| {
                            Err(ubm_desktop::DesktopError::cancelled(
                                "tauri.reclaim.revoked",
                            ))
                        }),
                    )
                    .is_err()
                {
                    // Keep the exact reservation as retryable debt.
                    continue;
                }
            }
            if matches!(
                slot.reservation.status(
                    slot.native_id,
                    &slot.capability,
                    &slot.native_lease,
                    &slot.caller_key
                ),
                Ok(Status::Released { .. } | Status::Retired { .. } | Status::Failed(_))
            ) {
                if let Some(current) = self
                    .reclaim_state
                    .lock()
                    .expect("reclaim state poisoned")
                    .slots
                    .get_mut(&key)
                {
                    current.settled = true;
                }
            }
        }
    }
    pub(super) fn marker_reclaim_authorized(
        &self,
        caller: &AuthenticatedCaller,
        start: &str,
        payload: &std::collections::BTreeMap<String, IpcValue>,
    ) -> bool {
        let Some(handle) = payload.get("reclaimHandle").and_then(super::as_string) else {
            return false;
        };
        let Some(session) = payload.get("__expectedLeaseId").and_then(super::as_string) else {
            return false;
        };
        let Some(generation) = payload
            .get("__expectedLeaseGeneration")
            .and_then(super::as_string)
        else {
            return false;
        };
        let key = (super::caller_key(caller), handle.to_owned());
        self.reclaim_state
            .lock()
            .expect("reclaim state poisoned")
            .mappings
            .get(&key)
            .is_some_and(|slot| {
                slot.valid
                    && slot.committed
                    && slot.start_handle == start
                    && slot.session == session
                    && slot.generation == generation
            })
    }
    pub(super) async fn ordered_reclaim_route(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let owner_key = super::caller_key(caller);
        let session =
            super::required_string(&payload, "__expectedLeaseId", "tauri.reclaim.session")?;
        let generation = super::required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.reclaim.session",
        )?;
        if command == "gatt.ordered-recording.reclaim.pending" {
            let after = match payload.get("after") {
                None | Some(IpcValue::Null) => None,
                Some(IpcValue::String(value)) => {
                    let n: u64 = value.parse().map_err(|_| malformed())?;
                    if n.to_string() != *value {
                        return Err(malformed());
                    }
                    Some(n)
                }
                _ => return Err(malformed()),
            };
            let limit = match payload.get("limit") {
                None => 64,
                Some(IpcValue::Number(n)) => n
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| (1..=64).contains(n))
                    .ok_or_else(malformed)?,
                _ => return Err(malformed()),
            };
            let state = self.reclaim_state.lock().expect("reclaim state poisoned");
            let mut handles: Vec<u64> = state
                .slots
                .iter()
                .filter_map(|((owner, handle), slot)| {
                    (owner == &owner_key && !slot.valid)
                        .then(|| handle.parse::<u64>().ok())
                        .flatten()
                })
                .filter(|n| after.is_none_or(|after| *n > after))
                .collect();
            handles.sort_unstable();
            let truncated = handles.len() > limit;
            handles.truncate(limit);
            let next = if truncated {
                handles
                    .last()
                    .map(|n| super::string(n.to_string()))
                    .unwrap_or(IpcValue::Null)
            } else {
                IpcValue::Null
            };
            let transactions = handles
                .into_iter()
                .map(|n| {
                    super::object([
                        ("reclaimHandle", super::string(n.to_string())),
                        ("state", super::string("revoked")),
                    ])
                })
                .collect();
            return Ok(super::object([
                ("transactions", IpcValue::Array(transactions)),
                ("truncated", IpcValue::Bool(truncated)),
                ("nextCursor", next),
            ]));
        }
        if command == "gatt.ordered-recording.recovery" {
            if !self.ensure_authority().await?.recording_reclaim_supported() {
                return Err(DispatchError::new(
                    BleErrorCode::CapabilityUnsupported,
                    "gatt",
                    "tauri.reclaim.unavailable",
                ));
            }
            let start = super::required_string(&payload, "startHandle", "tauri.reclaim.recovery")?;
            let slot = self
                .ordered_recording_starts
                .lock()
                .await
                .get(&(owner_key.clone(), start))
                .cloned()
                .ok_or_else(denied)?;
            let state = slot.state.lock().await;
            if state.issuer_lease_id != session
                || state.issuer_lease_generation != generation
                || self.is_revoked(&owner_key)
            {
                return Err(denied());
            }
            let token = state
                .owner
                .as_ref()
                .ok_or_else(denied)?
                .issue_recovery()
                .map_err(|_| denied())?;
            return Ok(super::object([("recoveryToken", super::string(token))]));
        }
        if command == "gatt.ordered-recording.reclaim.reserve" {
            let _gate = self.reclaim_admission.lock().await;
            let capability =
                super::required_string(&payload, "recoveryToken", "tauri.reclaim.capability")?;
            let caller_key = super::required_string(&payload, "callerKey", "tauri.reclaim.caller")?;
            let database =
                super::required_string(&payload, "databaseHandle", "tauri.reclaim.database")?;
            {
                let state = self.reclaim_state.lock().expect("reclaim state poisoned");
                if let Some(((.., handle), slot)) = state
                    .slots
                    .iter()
                    .find(|((owner, _), slot)| owner == &owner_key && slot.caller_key == caller_key)
                {
                    if slot.capability != capability
                        || slot.database != database
                        || slot.session != session
                        || slot.generation != generation
                        || !slot.valid
                    {
                        return Err(denied());
                    }
                    return Ok(super::object([(
                        "reclaimHandle",
                        super::string(handle.clone()),
                    )]));
                }
                if state.slots.len() >= super::MAX_PENDING_EVENTS {
                    return Err(DispatchError::new(
                        BleErrorCode::StreamQuota,
                        "gatt",
                        "tauri.reclaim.quota",
                    ));
                }
            }
            let starts: Vec<_> = self
                .ordered_recording_starts
                .lock()
                .await
                .iter()
                .filter(|((owner, _), _)| owner == &owner_key)
                .map(|((_, handle), slot)| (handle.clone(), slot.clone()))
                .collect();
            let mut matched = None;
            for (handle, slot) in starts {
                let state = slot.state.lock().await;
                if let Some(recording) = &state.owner {
                    if recording.validate_recovery(&capability).is_ok() {
                        matched = Some((
                            handle,
                            slot.recording_id.clone(),
                            state.peer.clone(),
                            state.connection_generation.clone(),
                            state.database_generation.clone(),
                            recording.clone(),
                        ));
                        break;
                    }
                }
            }
            let (start_handle, recording_id, peer, core_connection, core_database, recording) =
                matched.ok_or_else(denied)?;
            let reservation = recording
                .reclaim_reservation(&capability)
                .await
                .map_err(|_| denied())?;
            let request_id = reservation
                .next(&capability)
                .map_err(|e| DispatchError::from_core(&e))?;
            let handle = self
                .next_id
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| n.checked_add(1),
                )
                .map_err(|_| denied())?
                .to_string();
            let records = self
                .ensure_authority()
                .await?
                .ordered_peer_records()
                .await
                .map_err(|e| DispatchError::from_core(&e))?;
            if !records.iter().any(|r| {
                r.peer_id == peer
                    && r.connection_generation.as_deref() == Some(&core_connection)
                    && r.database_generation.as_deref() == Some(&core_database)
            }) {
                return Err(denied());
            }
            let mut state = self.inner.lock().await;
            let caller_state = state
                .callers
                .get_mut(&owner_key)
                .filter(|s| {
                    !s.retired
                        && s.lease_id == session
                        && s.lease_generation == generation
                        && !self.is_revoked(&owner_key)
                })
                .ok_or_else(denied)?;
            let db = caller_state
                .databases
                .get(&database)
                .filter(|d| d.valid)
                .ok_or_else(denied)?;
            super::validate_database_identity(&payload, db, "tauri.reclaim.database")?;
            let connection = caller_state
                .connections
                .get(&db.connection_handle)
                .ok_or_else(denied)?;
            super::validate_connection_identity(
                &payload,
                connection,
                &caller_state.lease_id,
                "tauri.reclaim.connection",
            )?;
            if connection.peer_id != peer || connection.core_generation != core_connection {
                return Err(denied());
            }
            let native_lease = connection.lease.clone();
            let connection_handle = db.connection_handle.clone();
            let slot = Slot {
                owner: owner_key.clone(),
                session,
                generation,
                database,
                connection: connection_handle,
                start_handle,
                recording_id,
                capability,
                native_id: request_id,
                native_lease,
                caller_key,
                reservation,
                valid: true,
                committed: false,
                settled: false,
                retired: false,
                acknowledged: false,
                forgotten: false,
            };
            slot.reservation
                .reserve(
                    slot.native_id,
                    &slot.capability,
                    &slot.native_lease,
                    &slot.caller_key,
                )
                .map_err(|e| DispatchError::from_core(&e))?;
            self.reclaim_state
                .lock()
                .expect("reclaim state poisoned")
                .slots
                .insert((owner_key, handle.clone()), slot);
            drop(state);
            return Ok(super::object([("reclaimHandle", super::string(handle))]));
        }
        if command == "gatt.ordered-recording.marker-owner" {
            let handle =
                super::required_string(&payload, "reclaimHandle", "tauri.reclaim.marker-owner")?;
            let recording =
                super::required_string(&payload, "recordingId", "tauri.reclaim.marker-owner")?;
            let database =
                super::required_string(&payload, "databaseHandle", "tauri.reclaim.marker-owner")?;
            let inner = self.inner.lock().await;
            let caller_state = inner
                .callers
                .get(&owner_key)
                .filter(|caller| !caller.retired && !self.is_revoked(&owner_key))
                .ok_or_else(denied)?;
            if caller_state.lease_id != session || caller_state.lease_generation != generation {
                return Err(denied());
            }
            let current_database = caller_state
                .databases
                .get(&database)
                .filter(|database| database.valid)
                .ok_or_else(denied)?;
            super::validate_database_identity(
                &payload,
                current_database,
                "tauri.reclaim.marker-owner",
            )?;
            let connection = caller_state
                .connections
                .get(&current_database.connection_handle)
                .ok_or_else(denied)?;
            super::validate_connection_identity(
                &payload,
                connection,
                &caller_state.lease_id,
                "tauri.reclaim.marker-owner",
            )?;
            let state = self.reclaim_state.lock().expect("reclaim state poisoned");
            let slot = state
                .mappings
                .get(&(owner_key, handle))
                .filter(|s| {
                    s.valid
                        && s.committed
                        && s.recording_id == recording
                        && s.database == database
                        && s.session == session
                        && s.generation == generation
                })
                .ok_or_else(denied)?;
            return Ok(super::object([(
                "startHandle",
                super::string(slot.start_handle.clone()),
            )]));
        }
        let _gate = self.reclaim_admission.lock().await;
        let handle = super::required_string(&payload, "reclaimHandle", "tauri.reclaim.handle")?;
        let key = (owner_key.clone(), handle.clone());
        let slot = {
            let state = self.reclaim_state.lock().expect("reclaim state poisoned");
            match state.slots.get(&key) {
                Some(slot)
                    if (slot.session == session && slot.generation == generation)
                        || (!slot.valid
                            && matches!(
                                command.rsplit('.').next(),
                                Some(
                                    "status"
                                        | "cancel"
                                        | "release"
                                        | "retire"
                                        | "ack-retirement"
                                        | "forget-retirement"
                                        | "confirm-forgotten"
                                )
                            )) =>
                {
                    slot.clone()
                }
                None if command.ends_with("confirm-forgotten")
                    && !state.slots.keys().any(|(_, other)| other == &handle) =>
                {
                    return Ok(super::object([("confirmed", IpcValue::Bool(true))]))
                }
                _ => return Err(denied()),
            }
        };
        let action = command.rsplit('.').next().ok_or_else(malformed)?;
        if action == "confirm-forgotten" {
            if !slot.forgotten {
                return Err(denied());
            }
            self.reclaim_state
                .lock()
                .expect("reclaim state poisoned")
                .slots
                .remove(&key);
            return Ok(super::object([("confirmed", IpcValue::Bool(true))]));
        }
        if !slot.valid
            && !matches!(
                action,
                "status" | "cancel" | "release" | "retire" | "ack-retirement" | "forget-retirement"
            )
        {
            return Err(denied());
        }
        let already_completed = match action {
            "retire" => slot.retired,
            "ack-retirement" => slot.acknowledged,
            "forget-retirement" => slot.forgotten,
            _ => false,
        };
        if action != "status" && !already_completed {
            let table = self.reclaim_state.clone();
            let revoked = self.revoked_callers.clone();
            let publish_key = key.clone();
            let publish = Box::new(move || {
                let mut state = table.lock().expect("reclaim state poisoned");
                if revoked
                    .lock()
                    .expect("revocation mutex poisoned")
                    .contains_key(&publish_key.0)
                {
                    return Err(ubm_desktop::DesktopError::cancelled(
                        "gatt.ordered.reclaim.commit",
                    ));
                }
                let slot = state
                    .slots
                    .get_mut(&publish_key)
                    .filter(|slot| slot.valid)
                    .ok_or_else(|| {
                        ubm_desktop::DesktopError::cancelled("gatt.ordered.reclaim.commit")
                    })?;
                slot.committed = true;
                let copy = slot.clone();
                state.mappings.insert(publish_key, copy);
                Ok(())
            });
            slot.reservation
                .action(
                    action,
                    slot.native_id,
                    &slot.capability,
                    &slot.native_lease,
                    &slot.caller_key,
                    publish,
                )
                .map_err(|e| DispatchError::from_core(&e))?;
        }
        if matches!(
            slot.reservation.status(
                slot.native_id,
                &slot.capability,
                &slot.native_lease,
                &slot.caller_key
            ),
            Ok(Status::Released { .. } | Status::Retired { .. } | Status::Failed(_))
        ) {
            if let Some(current) = self
                .reclaim_state
                .lock()
                .expect("reclaim state poisoned")
                .slots
                .get_mut(&key)
            {
                current.settled = true;
            }
        }
        match action {
            "retire" | "ack-retirement" => {
                if let Some(s) = self
                    .reclaim_state
                    .lock()
                    .expect("reclaim state poisoned")
                    .slots
                    .get_mut(&key)
                {
                    if action == "retire" {
                        s.retired = true;
                    } else {
                        s.acknowledged = true;
                    }
                }
                Ok(super::object([(
                    if action == "retire" {
                        "retired"
                    } else {
                        "acknowledged"
                    },
                    IpcValue::Bool(true),
                )]))
            }
            "forget-retirement" => {
                if let Some(s) = self
                    .reclaim_state
                    .lock()
                    .expect("reclaim state poisoned")
                    .slots
                    .get_mut(&key)
                {
                    s.forgotten = true;
                }
                Ok(super::object([("forgotten", IpcValue::Bool(true))]))
            }
            "start" | "status" | "commit" | "cancel" | "release" => Ok(super::object([(
                "status",
                receipt(
                    slot.reservation
                        .status(
                            slot.native_id,
                            &slot.capability,
                            &slot.native_lease,
                            &slot.caller_key,
                        )
                        .map_err(|e| DispatchError::from_core(&e))?,
                ),
            )])),
            _ => Err(malformed()),
        }
    }
}
