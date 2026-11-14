// native/tauri/src/ordered_recordings.rs
//! Process-owned ordered recording starts and their connection leases.

use super::{BtleplugDispatcher, DispatchError};

#[derive(Default)]
pub(super) struct Reservations {
    by_token: std::collections::HashMap<(String, String), String>,
    by_handle: std::collections::HashMap<(String, String), String>,
}
use crate::{AuthenticatedCaller, IpcValue};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
use tokio::sync::Mutex;
use ubm_core::contracts::BleErrorCode;
use ubm_desktop::continuation_journal::JournalQuota;
use ubm_desktop::ordered_recording::{
    stop_receipt, OrderedRecordingDeclaration, OrderedRecordingMember, RecordingError,
};
use ubm_desktop::{DeliveryMode, OpControl};

pub(super) struct StartSlot {
    pub(super) recording_id: String,
    pub(super) state: Arc<Mutex<Start>>,
}
pub(super) struct Start {
    pub(super) owner: Option<Arc<dyn crate::desktop_core::OrderedRecordingOwner>>,
    fingerprint: Vec<Option<IpcValue>>,
    pub(super) peer: String,
    pub(super) connection_generation: String,
    pub(super) database_generation: String,
    pub(super) issuer_lease_id: String,
    pub(super) issuer_lease_generation: String,
    result: Option<IpcValue>,
    has_owner: bool,
    collection_id: Option<u64>,
    finished: bool,
    retired: bool,
}

fn invalid() -> DispatchError {
    DispatchError::new(
        BleErrorCode::ProtocolMalformed,
        "ipc",
        "tauri.ordered-recording",
    )
}
pub(super) fn json_ipc(value: serde_json::Value) -> IpcValue {
    match value {
        serde_json::Value::Null => IpcValue::Null,
        serde_json::Value::Bool(v) => IpcValue::Bool(v),
        serde_json::Value::Number(v) => IpcValue::Number(v),
        serde_json::Value::String(v) => IpcValue::String(v),
        serde_json::Value::Array(v) => IpcValue::Array(v.into_iter().map(json_ipc).collect()),
        serde_json::Value::Object(v) => {
            IpcValue::Object(v.into_iter().map(|(k, v)| (k, json_ipc(v))).collect())
        }
    }
}
fn field_number(
    map: &BTreeMap<String, IpcValue>,
    name: &str,
    default: Option<u64>,
) -> Result<u64, DispatchError> {
    match map.get(name) {
        Some(IpcValue::Number(n)) => n.as_u64().filter(|n| *n > 0).ok_or_else(invalid),
        None => default.ok_or_else(invalid),
        _ => Err(invalid()),
    }
}
fn fingerprint(payload: &BTreeMap<String, IpcValue>) -> Vec<Option<IpcValue>> {
    [
        "databaseHandle",
        "databaseId",
        "members",
        "recording",
        "streamItemCapacity",
        "streamByteCapacity",
        "streamReservedControlCapacity",
        "connectionHandle",
        "peerId",
        "ownerLeaseId",
        "connectionId",
        "connectionGeneration",
        "databaseGeneration",
    ]
    .iter()
    .map(|key| payload.get(*key).cloned())
    .collect()
}

impl BtleplugDispatcher {
    pub(super) async fn recover_ordered_recording_starts(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let after = match payload.get("after") {
            None | Some(IpcValue::Null) => None,
            Some(IpcValue::String(value)) => {
                let n: u64 = value.parse().map_err(|_| invalid())?;
                if n.to_string() != *value {
                    return Err(invalid());
                }
                Some(n)
            }
            _ => return Err(invalid()),
        };
        let limit = match payload.get("limit") {
            None => 64,
            Some(IpcValue::Number(n)) => n
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=64).contains(n))
                .ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
        let owner = super::caller_key(caller);
        let mut slots: Vec<_> = self
            .ordered_recording_starts
            .lock()
            .await
            .iter()
            .filter(|((key, _), _)| key == &owner)
            .map(|((_, handle), slot)| (handle.clone(), slot.clone()))
            .collect();
        slots.sort_by_key(|(handle, _)| handle.parse::<u64>().unwrap_or(u64::MAX));
        let mut entries = Vec::new();
        let mut cursor = None;
        for (handle, slot) in slots
            .into_iter()
            .filter(|(handle, _)| {
                after.is_none_or(|after| handle.parse::<u64>().is_ok_and(|n| n > after))
            })
            .take(limit + 1)
        {
            cursor = Some(handle.clone());
            let state = match slot.state.try_lock() {
                Err(_) => "pending",
                Ok(state) if state.retired => "retired",
                Ok(state) if state.result.is_some() => "result",
                Ok(_) => "pending",
            };
            entries.push(super::object([
                ("startHandle", super::string(handle)),
                ("recordingId", super::string(slot.recording_id.clone())),
                ("state", super::string(state)),
            ]));
        }
        let truncated = entries.len() > limit;
        if truncated {
            entries.pop();
            cursor = entries
                .last()
                .and_then(|entry| match entry {
                    IpcValue::Object(fields) => {
                        fields.get("startHandle").and_then(super::as_string)
                    }
                    _ => None,
                })
                .map(str::to_owned);
        }
        Ok(super::object([
            ("starts", IpcValue::Array(entries)),
            ("truncated", IpcValue::Bool(truncated)),
            (
                "nextCursor",
                if truncated {
                    cursor.map(super::string).unwrap_or(IpcValue::Null)
                } else {
                    IpcValue::Null
                },
            ),
        ]))
    }
    pub(super) async fn reserve_ordered_recording_start(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        use std::sync::atomic::Ordering;
        let token = super::required_string(
            &payload,
            "reservationToken",
            "tauri.ordered-recording.reserve",
        )?;
        if token.len() > 256 {
            return Err(invalid());
        }
        let owner = super::caller_key(caller);
        let expected_lease = super::required_string(
            &payload,
            "__expectedLeaseId",
            "tauri.ordered-recording.reserve",
        )?;
        let expected_generation = super::required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.ordered-recording.reserve",
        )?;
        let mut reservations = self.ordered_recording_reservations.lock().await;
        let slots = self.ordered_recording_starts.lock().await;
        let state = self.inner.lock().await;
        if self.is_revoked(&owner)
            || !state.callers.get(&owner).is_some_and(|caller| {
                !caller.retired
                    && caller.lease_id == expected_lease
                    && caller.lease_generation == expected_generation
            })
        {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.ordered-recording.reserve-owner",
            ));
        }
        if let Some(handle) = reservations.by_token.get(&(owner.clone(), token.clone())) {
            return Ok(super::object([(
                "startHandle",
                super::string(handle.clone()),
            )]));
        }
        let retired_live = slots
            .keys()
            .filter(|key| !reservations.by_handle.contains_key(*key))
            .count();
        if reservations.by_handle.len() + retired_live >= super::MAX_PENDING_EVENTS {
            return Err(DispatchError::new(
                BleErrorCode::StreamQuota,
                "gatt",
                "tauri.ordered-recording.quota",
            ));
        }
        let handle = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "gatt",
                    "tauri.ordered-recording.identifiers",
                )
            })?
            .to_string();
        reservations
            .by_token
            .insert((owner.clone(), token.clone()), handle.clone());
        reservations
            .by_handle
            .insert((owner, handle.clone()), token);
        Ok(super::object([("startHandle", super::string(handle))]))
    }
    pub(super) async fn retire_unused_ordered_recording_reservations(&self, owner: &str) {
        let mut reservations = self.ordered_recording_reservations.lock().await;
        let slots = self.ordered_recording_starts.lock().await;
        let unused: Vec<_> = reservations
            .by_handle
            .keys()
            .filter(|key| key.0 == owner && !slots.contains_key(*key))
            .cloned()
            .collect();
        for key in unused {
            if let Some(token) = reservations.by_handle.remove(&key) {
                reservations.by_token.remove(&(key.0, token));
            }
        }
    }
    pub(super) async fn start_ordered_recording(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle =
            super::required_string(&payload, "startHandle", "tauri.ordered-recording.start")?;
        let issuer_lease_id = super::required_string(
            &payload,
            "__expectedLeaseId",
            "tauri.ordered-recording.start",
        )?;
        let issuer_lease_generation = super::required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.ordered-recording.start",
        )?;
        let db_handle =
            super::required_string(&payload, "databaseHandle", "tauri.ordered-recording.start")?;
        let fp = fingerprint(&payload);
        let key = super::caller_key(caller);
        let slot_key = (key.clone(), handle.clone());
        if !self
            .ordered_recording_reservations
            .lock()
            .await
            .by_handle
            .contains_key(&slot_key)
        {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.ordered-recording.reservation",
            ));
        }
        if let Some(slot) = self
            .ordered_recording_starts
            .lock()
            .await
            .get(&slot_key)
            .cloned()
        {
            let slot = slot.state.lock().await;
            if slot.retired || slot.fingerprint != fp {
                return Err(invalid());
            }
            return slot.result.clone().ok_or_else(invalid);
        }
        let rec = match payload.get("recording") {
            Some(IpcValue::Object(value)) => value,
            _ => return Err(invalid()),
        };
        let recording_id = super::required_string(rec, "id", "tauri.ordered-recording.id")?;
        if recording_id.len() > 64
            || !recording_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(invalid());
        }
        let quota = JournalQuota {
            max_bytes: field_number(rec, "maxBytes", None)?,
            max_records: field_number(rec, "maxRecords", None)?,
        };
        let raw = match payload.get("members") {
            Some(IpcValue::Array(v)) if !v.is_empty() => v,
            _ => return Err(invalid()),
        };
        let max_items = usize::try_from(field_number(&payload, "streamItemCapacity", Some(128))?)
            .map_err(|_| invalid())?;
        let max_bytes = usize::try_from(field_number(&payload, "streamByteCapacity", Some(65536))?)
            .map_err(|_| invalid())?;
        let max_controls = usize::try_from(field_number(
            &payload,
            "streamReservedControlCapacity",
            Some(1),
        )?)
        .map_err(|_| invalid())?;
        let authority = self.ensure_authority().await?;
        let (peer, core_connection, source_lease, selectors) = {
            let state = self.inner.lock().await;
            let owner = state
                .callers
                .get(&key)
                .filter(|v| !v.retired)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered-recording.owner",
                    )
                })?;
            let db = owner
                .databases
                .get(&db_handle)
                .filter(|db| db.valid)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "gatt",
                        "tauri.ordered-recording.database",
                    )
                })?;
            super::validate_database_identity(&payload, db, "tauri.ordered-recording.database")?;
            let conn = owner
                .connections
                .get(&db.connection_handle)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::ConnectionStale,
                        "connection",
                        "tauri.ordered-recording.connection",
                    )
                })?;
            super::validate_connection_identity(
                &payload,
                conn,
                &owner.lease_id,
                "tauri.ordered-recording.connection",
            )?;
            let mut ids = HashSet::new();
            let mut paths = HashSet::new();
            let mut members = Vec::new();
            for (index, item) in raw.iter().enumerate() {
                let item = match item {
                    IpcValue::Object(v) => v,
                    _ => return Err(invalid()),
                };
                let public =
                    super::required_string(item, "memberId", "tauri.ordered-recording.member")?;
                let characteristic = super::required_string(
                    item,
                    "characteristicHandle",
                    "tauri.ordered-recording.member",
                )?;
                if !ids.insert(public.clone()) || !paths.insert(characteristic.clone()) {
                    return Err(invalid());
                }
                let characteristic = db.characteristics.get(&characteristic).ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattNotFound,
                        "gatt",
                        "tauri.ordered-recording.member",
                    )
                })?;
                let mode = super::required_string(
                    item,
                    "deliveryMode",
                    "tauri.ordered-recording.delivery",
                )?;
                let (delivery, preference) = match mode.as_str() {
                    "require-notification" => (Some(DeliveryMode::Notification), None),
                    "require-indication" => (Some(DeliveryMode::Indication), None),
                    "prefer-notification" => (None, Some(DeliveryMode::Notification)),
                    "prefer-indication" => (None, Some(DeliveryMode::Indication)),
                    _ => return Err(invalid()),
                };
                members.push(OrderedRecordingMember {
                    member_id: u64::try_from(index).map_err(|_| invalid())?,
                    public_member_id: public,
                    selector: authority
                        .ordered_selector(&characteristic.selector)
                        .map_err(|e| DispatchError::from_core(&e))?,
                    delivery,
                    preference,
                });
            }
            (
                conn.peer_id.clone(),
                conn.core_generation.clone(),
                conn.lease.clone(),
                members,
            )
        };
        let records = authority
            .ordered_peer_records()
            .await
            .map_err(|e| DispatchError::from_core(&e))?;
        let database = records
            .into_iter()
            .find(|r| {
                r.peer_id == peer && r.connection_generation.as_deref() == Some(&core_connection)
            })
            .and_then(|r| r.database_generation)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::GattStaleHandle,
                    "gatt",
                    "tauri.ordered-recording.generation",
                )
            })?;
        let declaration = OrderedRecordingDeclaration {
            peer_id: peer.clone(),
            connection_generation: core_connection.clone(),
            database_generation: database.clone(),
            members: selectors,
            max_items,
            max_bytes,
            max_controls,
        };
        let slot = Arc::new(StartSlot {
            recording_id: recording_id.clone(),
            state: Arc::new(Mutex::new(Start {
                owner: None,
                fingerprint: fp,
                peer: peer.clone(),
                connection_generation: core_connection.clone(),
                database_generation: database,
                issuer_lease_id,
                issuer_lease_generation,
                result: None,
                has_owner: false,
                collection_id: None,
                finished: false,
                retired: false,
            })),
        });
        let guard = slot
            .state
            .clone()
            .try_lock_owned()
            .expect("new recording slot");
        {
            let reservations = self.ordered_recording_reservations.lock().await;
            if !reservations.by_handle.contains_key(&slot_key) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.ordered-recording.reservation",
                ));
            }
            let mut slots = self.ordered_recording_starts.lock().await;
            if slots.contains_key(&slot_key)
                || slots.values().any(|s| s.recording_id == recording_id)
            {
                return Err(invalid());
            }
            if slots.len() >= super::MAX_PENDING_EVENTS {
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "gatt",
                    "tauri.ordered-recording.quota",
                ));
            }
            let state = self.inner.lock().await;
            let owner = state
                .callers
                .get(&key)
                .filter(|o| !o.retired && !self.is_revoked(&key))
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered-recording.owner",
                    )
                })?;
            let db = owner
                .databases
                .get(&db_handle)
                .filter(|db| db.valid)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "gatt",
                        "tauri.ordered-recording.database",
                    )
                })?;
            super::validate_database_identity(&payload, db, "tauri.ordered-recording.database")?;
            let connection = owner
                .connections
                .get(&db.connection_handle)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::ConnectionStale,
                        "connection",
                        "tauri.ordered-recording.connection",
                    )
                })?;
            super::validate_connection_identity(
                &payload,
                connection,
                &owner.lease_id,
                "tauri.ordered-recording.connection",
            )?;
            if connection.core_generation != core_connection {
                return Err(invalid());
            }
            if ctl.ticket.is_cancel_requested() {
                return Err(DispatchError::from_core(
                    &ubm_desktop::DesktopError::cancelled("gatt.ordered-recording.start"),
                ));
            }
            slots.insert(slot_key, slot.clone());
        }
        let registry = self.recordings.clone();
        #[cfg(test)]
        let panic_phase = self.ordered_recording_panic_phase.clone();
        let worker = tokio::spawn(async move {
            let mut slot = guard;
            #[cfg(test)]
            if panic_phase.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                panic!("scripted recording worker failure before lease");
            }
            let result = match authority
                .prepare_ordered_recording(registry, recording_id.clone(), quota, declaration)
                .await
            {
                Err(error) => {
                    slot.finished = true;
                    serde_json::json!({"kind":"prepare-failed","receipt":ubm_desktop::ordered_recording::prepare_failure_receipt(&recording_id, &error)})
                }
                Ok(owner) => {
                    slot.has_owner = true;
                    slot.owner = Some(owner.clone());
                    let collection = owner.collection_id();
                    slot.collection_id = Some(collection);
                    #[cfg(test)]
                    if panic_phase.load(std::sync::atomic::Ordering::SeqCst) == 2 {
                        panic!("scripted recording worker failure after owner");
                    }
                    let receipt = owner
                        .start_receipt(ctl.with_connection_lease(source_lease))
                        .await;
                    #[cfg(test)]
                    if panic_phase.load(std::sync::atomic::Ordering::SeqCst) == 3 {
                        panic!("scripted recording worker failure after source start");
                    }
                    match receipt {
                        Ok(receipt) => serde_json::json!({"kind":"started","receipt":receipt}),
                        Err(error) => ubm_desktop::ordered_recording::owner_failure_receipt(
                            &recording_id,
                            collection,
                            &error,
                        ),
                    }
                }
            };
            slot.result = Some(json_ipc(result));
            slot.result.clone()
        });
        let starts = self.ordered_recording_starts.clone();
        let task = tokio::spawn(async move {
            match worker.await {
                Ok(result) => result,
                Err(_) => {
                    let mut state = slot.state.lock().await;
                    let result = if state.has_owner {
                        ubm_desktop::ordered_recording::owner_failure_receipt(
                            &slot.recording_id,
                            state
                                .collection_id
                                .expect("owned recording has collection identity"),
                            &RecordingError::WorkerFailed,
                        )
                    } else {
                        state.finished = true;
                        let result = serde_json::json!({"kind":"prepare-failed","receipt":ubm_desktop::ordered_recording::prepare_failure_receipt(&slot.recording_id, &RecordingError::WorkerFailed)});
                        result
                    };
                    state.result = Some(json_ipc(result));
                    let result = state.result.clone();
                    let reclaim = state.retired && state.finished;
                    drop(state);
                    if reclaim {
                        starts
                            .lock()
                            .await
                            .retain(|_, candidate| !Arc::ptr_eq(candidate, &slot));
                    }
                    result
                }
            }
        });
        task.await.map_err(|_| invalid())?.ok_or_else(invalid)
    }

    pub(super) async fn ordered_recording_start_status(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle =
            super::required_string(&payload, "startHandle", "tauri.ordered-recording.status")?;
        let key = (super::caller_key(caller), handle.clone());
        if !self
            .ordered_recording_reservations
            .lock()
            .await
            .by_handle
            .contains_key(&key)
        {
            return Ok(super::object([("state", super::string("unknown"))]));
        }
        let slot = self
            .ordered_recording_starts
            .lock()
            .await
            .get(&(super::caller_key(caller), handle))
            .cloned();
        let Some(slot) = slot else {
            return Ok(super::object([("state", super::string("unknown"))]));
        };
        let Ok(slot) = slot.state.try_lock() else {
            return Ok(super::object([("state", super::string("pending"))]));
        };
        if slot.retired {
            return Ok(super::object([("state", super::string("unknown"))]));
        }
        match &slot.result {
            Some(result) => Ok(super::object([
                ("state", super::string("result")),
                ("result", result.clone()),
            ])),
            None => Ok(super::object([("state", super::string("pending"))])),
        }
    }
    pub(super) async fn retire_ordered_recording_start(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle =
            super::required_string(&payload, "startHandle", "tauri.ordered-recording.retire")?;
        let key = (super::caller_key(caller), handle);
        let mut reservations = self.ordered_recording_reservations.lock().await;
        if let Some(token) = reservations.by_handle.remove(&key) {
            reservations.by_token.remove(&(key.0.clone(), token));
        } else {
            return Ok(super::object([("retired", IpcValue::Bool(true))]));
        }
        let mut slots = self.ordered_recording_starts.lock().await;
        if let Some(slot) = slots.get(&key).cloned() {
            let mut slot = slot.state.lock().await;
            slot.retired = true;
            slot.result = None;
            if slot.finished {
                slots.remove(&key);
            }
        }
        Ok(super::object([("retired", IpcValue::Bool(true))]))
    }
    pub(super) async fn stop_owned_ordered_recording(
        &self,
        id: &str,
    ) -> Option<Result<serde_json::Value, serde_json::Value>> {
        let slots: Vec<_> = self
            .ordered_recording_starts
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        for slot_ref in slots {
            if slot_ref.recording_id != id {
                continue;
            }
            let mut slot = slot_ref.state.lock().await;
            let receipt = if slot.has_owner {
                match self.recordings.stop_ordered_host_receipt(id).await {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                }
            } else {
                serde_json::json!({"recording":stop_receipt(Err(RecordingError::MissingTerminal)),"hostCleanup":{"state":"not-applicable"}})
            };
            if slot.has_owner
                && (receipt["recording"]["state"] != "finished"
                    || !matches!(
                        receipt["hostCleanup"]["state"].as_str(),
                        Some("released" | "not-applicable")
                    ))
            {
                return Some(Ok(receipt));
            }
            slot.finished = true;
            let retired = slot.retired;
            drop(slot);
            if retired {
                let mut slots = self.ordered_recording_starts.lock().await;
                slots.retain(|_, candidate| !Arc::ptr_eq(candidate, &slot_ref));
            }
            return Some(Ok(receipt));
        }
        None
    }
}
