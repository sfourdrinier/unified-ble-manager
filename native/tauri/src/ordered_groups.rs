// native/tauri/src/ordered_groups.rs
//! Retained Tauri ownership for one connection-scoped ordered group.

use super::ordered_delivery::OrderedDelivery;
use crate::IpcValue;

pub(super) struct OrphanSlot {
    pub(super) owner: String,
    pub(super) id: String,
    pub(super) receipt: Option<IpcValue>,
    pub(super) page_start: usize,
    pub(super) page_end: Option<usize>,
    pub(super) page_done: bool,
}

#[derive(Default)]
pub(super) struct OrphanProgress {
    pub(super) id: Option<String>,
    pub(super) values: u64,
    pub(super) bytes: u64,
    pub(super) controls: u64,
    pub(super) uncertain: bool,
    pub(super) losses: Vec<serde_json::Value>,
    pub(super) failure_observations: Vec<serde_json::Value>,
    pub(super) failure_observations_omitted: Vec<serde_json::Value>,
    pub(super) trusted_values: Vec<serde_json::Value>,
    pub(super) markers: Vec<serde_json::Value>,
    pub(super) marker_write: Option<(u64, serde_json::Value)>,
    pub(super) terminal: Option<serde_json::Value>,
    pub(super) source_closed: bool,
    pub(super) pending_receipt_ack: Option<u64>,
    pub(super) final_receipt: Option<IpcValue>,
    pub(super) core_acknowledged: bool,
    pub(super) error: Option<IpcValue>,
}

pub(super) struct OrderedGroup {
    pub(super) peer_id: String,
    pub(super) collection_id: u64,
    pub(super) connection_handle: String,
    pub(super) open_response: Option<IpcValue>,
    pub(super) delivery: OrderedDelivery,
    pub(super) pending_native_id: Option<u64>,
    pub(super) source_closed: bool,
    pub(super) released: bool,
    pub(super) core_acknowledged: bool,
    pub(super) core_forgotten: bool,
    pub(super) last_status: Option<String>,
    pub(super) terminal_status: Option<String>,
    pub(super) terminal_status_pending: bool,
    pub(super) member_ids: Vec<String>,
    pub(super) public_connection: String,
    pub(super) public_database: String,
    pub(super) orphan: OrphanProgress,
}

impl super::BtleplugDispatcher {
    pub(super) async fn open_ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
        ctl: ubm_desktop::OpControl,
    ) -> Result<IpcValue, super::DispatchError> {
        use ubm_core::contracts::BleErrorCode;
        let invalid = || {
            super::DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", "tauri.ordered.open")
        };
        let handle = super::required_string(&payload, "orderedHandle", "tauri.ordered.open")?;
        let database_handle =
            super::required_string(&payload, "databaseHandle", "tauri.ordered.open")?;
        let raw_members = match payload.get("members") {
            Some(IpcValue::Array(values)) if !values.is_empty() => values,
            _ => return Err(invalid()),
        };
        let limit = |key: &str, default: usize| -> Result<usize, super::DispatchError> {
            match payload.get(key) {
                None => Ok(default),
                Some(IpcValue::Number(value)) => value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(invalid),
                _ => Err(invalid()),
            }
        };
        let (max_items, max_bytes, max_controls) = (
            limit("streamItemCapacity", 128)?,
            limit("streamByteCapacity", 65536)?,
            limit("streamReservedControlCapacity", 1)?,
        );
        if max_bytes <= max_controls {
            return Err(invalid());
        }
        let authority = self.ensure_authority().await?;
        let key = super::caller_key(caller);
        let (
            peer,
            lease,
            core_connection,
            connection_handle,
            public_connection,
            public_database,
            selectors,
            member_ids,
        ) = {
            let state = self.inner.lock().await;
            let owner = state
                .callers
                .get(&key)
                .filter(|owner| !owner.retired)
                .ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered.owner",
                    )
                })?;
            if let Some(existing) = owner.ordered_groups.get(&handle) {
                let existing = existing.clone();
                drop(state);
                let existing = existing.lock().await;
                return existing.open_response.clone().ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::LifecycleInvalidState,
                        "gatt",
                        "tauri.ordered.open-pending",
                    )
                });
            }
            let database = owner
                .databases
                .get(&database_handle)
                .filter(|db| db.valid)
                .ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "gatt",
                        "tauri.ordered.database",
                    )
                })?;
            super::validate_database_identity(&payload, database, "tauri.ordered.database")?;
            let connection = owner
                .connections
                .get(&database.connection_handle)
                .ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::ConnectionStale,
                        "connection",
                        "tauri.ordered.connection",
                    )
                })?;
            super::validate_connection_identity(
                &payload,
                connection,
                &owner.lease_id,
                "tauri.ordered.connection",
            )?;
            let mut ids = std::collections::HashSet::new();
            let mut selected = std::collections::HashSet::new();
            let mut selectors = Vec::new();
            let mut member_ids = Vec::new();
            for (index, member) in raw_members.iter().enumerate() {
                let member = match member {
                    IpcValue::Object(value) => value,
                    _ => return Err(invalid()),
                };
                let member_id = super::required_string(member, "memberId", "tauri.ordered.member")?;
                let characteristic =
                    super::required_string(member, "characteristicHandle", "tauri.ordered.member")?;
                if !ids.insert(member_id.clone()) || !selected.insert(characteristic.clone()) {
                    return Err(invalid());
                }
                member_ids.push(member_id);
                let characteristic =
                    database
                        .characteristics
                        .get(&characteristic)
                        .ok_or_else(|| {
                            super::DispatchError::new(
                                BleErrorCode::GattNotFound,
                                "gatt",
                                "tauri.ordered.member",
                            )
                        })?;
                let mode =
                    super::required_string(member, "deliveryMode", "tauri.ordered.delivery")?;
                let (delivery, preference) = match mode.as_str() {
                    "require-notification" => (Some(ubm_desktop::DeliveryMode::Notification), None),
                    "require-indication" => (Some(ubm_desktop::DeliveryMode::Indication), None),
                    "prefer-notification" => (None, Some(ubm_desktop::DeliveryMode::Notification)),
                    "prefer-indication" => (None, Some(ubm_desktop::DeliveryMode::Indication)),
                    _ => return Err(invalid()),
                };
                selectors.push((
                    u64::try_from(index).map_err(|_| invalid())?,
                    characteristic.selector.clone(),
                    delivery,
                    preference,
                ));
            }
            (
                connection.peer_id.clone(),
                connection.lease.clone(),
                connection.core_generation.clone(),
                database.connection_handle.clone(),
                connection.connection_generation.clone(),
                database.database_generation.clone(),
                selectors,
                member_ids,
            )
        };
        let records = authority
            .ordered_peer_records()
            .await
            .map_err(|error| super::DispatchError::from_core(&error))?;
        let record = records
            .into_iter()
            .find(|record| {
                record.peer_id == peer
                    && record.connection_generation.as_deref() == Some(&core_connection)
            })
            .ok_or_else(|| {
                super::DispatchError::new(
                    BleErrorCode::ConnectionStale,
                    "connection",
                    "tauri.ordered.generation",
                )
            })?;
        let core_database = record.database_generation.ok_or_else(|| {
            super::DispatchError::new(
                BleErrorCode::GattStaleHandle,
                "gatt",
                "tauri.ordered.generation",
            )
        })?;
        let mut claims = Vec::new();
        for (id, selector, delivery, preference) in selectors {
            claims.push(ubm_desktop::central::OrderedClaimRequest {
                member_id: id,
                selector: authority
                    .ordered_selector(&selector)
                    .map_err(|error| super::DispatchError::from_core(&error))?,
                consumer: format!("tauri-ordered-{handle}-{id}"),
                delivery,
                preference,
            });
        }
        let (mut group, id) = {
            let mut state = self.inner.lock().await;
            let owner = state
                .callers
                .get_mut(&key)
                .filter(|owner| !owner.retired)
                .ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered.owner",
                    )
                })?;
            let database = owner
                .databases
                .get(&database_handle)
                .filter(|db| db.valid)
                .ok_or_else(|| {
                    super::DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "gatt",
                        "tauri.ordered.database",
                    )
                })?;
            super::validate_database_identity(&payload, database, "tauri.ordered.database")?;
            let connection = owner.connections.get(&connection_handle).ok_or_else(|| {
                super::DispatchError::new(
                    BleErrorCode::ConnectionStale,
                    "connection",
                    "tauri.ordered.connection",
                )
            })?;
            super::validate_connection_identity(
                &payload,
                connection,
                &owner.lease_id,
                "tauri.ordered.connection",
            )?;
            if connection.core_generation != core_connection
                || owner.ordered_groups.contains_key(&handle)
            {
                return Err(invalid());
            }
            if owner.ordered_groups.len() >= super::MAX_PENDING_EVENTS {
                return Err(super::DispatchError::new(
                    BleErrorCode::StreamQuota,
                    "gatt",
                    "tauri.ordered.quota",
                ));
            }
            // Cancellation and handle publication share this lock with status lookup.
            // Once status can observe `unknown` after cancellation, no group is reserved.
            if ctl.ticket.is_cancel_requested() {
                return Err(super::DispatchError::from_core(
                    &ubm_desktop::DesktopError::cancelled("gatt.ordered.open"),
                ));
            }
            let id = authority
                .reserve_ordered_batch(&peer)
                .map_err(|error| super::DispatchError::from_core(&error))?;
            let group = std::sync::Arc::new(tokio::sync::Mutex::new(OrderedGroup {
                peer_id: peer.clone(),
                collection_id: id,
                connection_handle,
                open_response: None,
                delivery: OrderedDelivery::default(),
                pending_native_id: None,
                source_closed: false,
                released: false,
                core_acknowledged: false,
                core_forgotten: false,
                last_status: None,
                terminal_status: None,
                terminal_status_pending: false,
                member_ids,
                public_connection: public_connection.clone(),
                public_database: public_database.clone(),
                orphan: OrphanProgress::default(),
            }));
            let locked = group
                .clone()
                .try_lock_owned()
                .expect("new group is unlocked");
            owner.ordered_groups.insert(handle.clone(), group);
            (locked, id)
        };
        let result = authority
            .open_ordered_batch(
                &peer,
                id,
                &core_connection,
                &core_database,
                &claims,
                max_items,
                max_bytes,
                max_controls,
                ctl.with_connection_lease(lease),
            )
            .await;
        let mut status: serde_json::Value = match native_status(authority.as_ref(), &group).await {
            Ok(status) => serde_json::from_str(&status).map_err(|_| invalid())?,
            Err(error) => {
                serde_json::json!({"members":[], "setupError":error.normalized_error().into_wire(), "fenceError":null, "sourceGeneration":null})
            }
        };
        if let Err(error) = result {
            status["setupError"] = normalized_error(&error)?;
        }
        if status["members"]
            .as_array()
            .is_none_or(|members| members.len() != claims.len())
        {
            status["members"] = serde_json::Value::Array(claims.iter().map(|claim| serde_json::json!({"memberId":claim.member_id.to_string(), "acquisition":"indeterminate", "effectiveDelivery":"unknown", "cleanupError":null, "cleanupReleased":false})).collect());
        }
        let response = super::object([
            ("handle", super::string(handle)),
            ("state", super::string("native")),
            ("nativeStatus", super::string(status.to_string())),
            ("collectionId", super::string(id.to_string())),
            ("connectionGeneration", super::string(public_connection)),
            ("databaseGeneration", super::string(public_database)),
            ("observations", IpcValue::Array(Vec::new())),
            ("uncertainTail", IpcValue::Bool(true)),
            ("cleanupState", super::string("pending-source")),
        ]);
        group.open_response = Some(response.clone());
        Ok(response)
    }

    pub(super) async fn ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: &std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<std::sync::Arc<tokio::sync::Mutex<OrderedGroup>>, super::DispatchError> {
        let handle = super::required_string(payload, "orderedHandle", "tauri.ordered.handle")?;
        let state = self.inner.lock().await;
        state
            .callers
            .get(&super::caller_key(caller))
            .and_then(|owner| owner.ordered_groups.get(&handle))
            .cloned()
            .ok_or_else(|| {
                super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.ordered.owner",
                )
            })
    }

    pub(super) async fn ordered_open_status(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let handle = super::required_string(&payload, "orderedHandle", "tauri.ordered.status")?;
        let group = self
            .inner
            .lock()
            .await
            .callers
            .get(&super::caller_key(caller))
            .and_then(|owner| owner.ordered_groups.get(&handle))
            .cloned();
        let Some(group) = group else {
            return Ok(super::object([("state", super::string("unknown"))]));
        };
        let Ok(group) = group.try_lock() else {
            return Ok(super::object([("state", super::string("pending"))]));
        };
        match &group.open_response {
            Some(result) => Ok(super::object([
                ("state", super::string("result")),
                ("result", result.clone()),
            ])),
            None => Ok(super::object([("state", super::string("pending"))])),
        }
    }

    pub(super) async fn next_ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let ack = match payload.get("ackDeliveryId") {
            None => None,
            Some(IpcValue::String(value)) if !value.is_empty() => Some(value.as_str()),
            _ => {
                return Err(super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.ordered.ack",
                ))
            }
        };
        let group = self.ordered_group(caller, &payload).await?;
        let authority = self.ensure_authority().await?;
        let mut group = group.lock().await;
        if ack.is_some()
            && group
                .delivery
                .retained()
                .is_some_and(|(id, _)| Some(id) == ack)
        {
            let native_id = group.pending_native_id.ok_or_else(|| {
                super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvariantViolation,
                    "gatt",
                    "tauri.ordered.delivery-identity",
                )
            })?;
            authority
                .acknowledge_ordered_delivery(&group.peer_id, group.collection_id, native_id)
                .await
                .map_err(|error| super::DispatchError::from_core(&error))?;
            group.pending_native_id = None;
        }
        group.delivery.acknowledge(ack).map_err(|()| {
            super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::ProtocolViolation,
                "ipc",
                "tauri.ordered.ack",
            )
        })?;
        if group.delivery.retained().is_some() {
            return retained_response(authority.as_ref(), &mut group).await;
        }
        if group.source_closed || group.released {
            group.source_closed = true;
            return Ok(super::object([("state", super::string("closed"))]));
        }
        // Test capacity before polling: a consumed native record must always have a retained slot.
        if !group.delivery.can_retain() {
            return Err(super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                "ipc",
                "tauri.ordered.delivery-id-exhausted",
            ));
        }
        let delivery = authority
            .poll_ordered_delivery(&group.peer_id, group.collection_id)
            .await
            .map_err(|e| super::DispatchError::from_core(&e))?;
        match delivery.poll {
            ubm_core::ordered_notifications::Poll::Empty => {
                Ok(super::object([("state", super::string("empty"))]))
            }
            ubm_core::ordered_notifications::Poll::Closed => {
                group.source_closed = true;
                Ok(super::object([("state", super::string("closed"))]))
            }
            record => {
                group.terminal_status_pending = true;
                let wire = json_value(ubm_desktop::ordered_wire::encode(record));
                let native_id = delivery.delivery_id.ok_or_else(|| {
                    super::DispatchError::new(
                        ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                        "gatt",
                        "tauri.ordered.delivery-identity",
                    )
                })?;
                group
                    .delivery
                    .retain_native(native_id, wire)
                    .map_err(|()| {
                        super::DispatchError::new(
                            ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                            "ipc",
                            "tauri.ordered.delivery",
                        )
                    })?;
                group.pending_native_id = Some(native_id);
                retained_response(authority.as_ref(), &mut group).await
            }
        }
    }
    pub(super) async fn retire_ordered_resource(
        &self,
        key: &str,
        handle: &str,
        group: std::sync::Arc<tokio::sync::Mutex<OrderedGroup>>,
    ) -> Result<(), super::DispatchError> {
        let mut locked = group.lock().await;
        let still_owned = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .and_then(|owner| owner.ordered_groups.get(handle))
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &group));
        if !still_owned {
            return Ok(());
        }
        let authority = self.ensure_authority().await?;
        if !locked.core_acknowledged {
            authority
                .acknowledge_ordered_batch(&locked.peer_id, locked.collection_id)
                .await
                .map_err(|error| super::DispatchError::from_core(&error))?;
            locked.core_acknowledged = true;
        }
        if !locked.core_forgotten {
            authority
                .forget_ordered_batch(&locked.peer_id, locked.collection_id)
                .await
                .map_err(|error| super::DispatchError::from_core(&error))?;
            locked.core_forgotten = true;
        }
        Ok(())
    }

    pub(super) async fn remove_ordered_mapping(
        &self,
        key: &str,
        handle: &str,
        group: &std::sync::Arc<tokio::sync::Mutex<OrderedGroup>>,
    ) {
        let mut state = self.inner.lock().await;
        if let Some(owner) = state.callers.get_mut(key) {
            if owner
                .ordered_groups
                .get(handle)
                .is_some_and(|stored| std::sync::Arc::ptr_eq(stored, group))
            {
                owner.ordered_groups.remove(handle);
            }
        }
    }

    pub(super) async fn retire_ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let handle = super::required_string(&payload, "orderedHandle", "tauri.ordered.retire")?;
        let key = super::caller_key(caller);
        let (group, foreign) = {
            let state = self.inner.lock().await;
            let group = state
                .callers
                .get(&key)
                .and_then(|owner| owner.ordered_groups.get(&handle))
                .cloned();
            let foreign = state.callers.iter().any(|(owner_key, owner)| {
                owner_key != &key && owner.ordered_groups.contains_key(&handle)
            });
            (group, foreign)
        };
        let Some(group) = group else {
            if foreign {
                return Err(super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.ordered.retire-owner",
                ));
            }
            return Ok(super::object([("state", super::string("retired"))]));
        };
        let locked = group.lock().await;
        if !locked.core_forgotten {
            return Err(super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                "gatt",
                "tauri.ordered.retire-pending",
            ));
        }
        self.remove_ordered_mapping(&key, &handle, &group).await;
        Ok(super::object([("state", super::string("retired"))]))
    }

    pub(super) async fn forget_ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, super::DispatchError> {
        let handle = super::required_string(&payload, "orderedHandle", "tauri.ordered.forget")?;
        let key = super::caller_key(caller);
        let group = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .and_then(|owner| owner.ordered_groups.get(&handle))
            .cloned();
        let Some(group) = group else {
            return Err(super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.ordered.forget-owner",
            ));
        };
        let locked = group.lock().await;
        if !locked.released || !locked.source_closed || locked.delivery.retained().is_some() {
            return Err(super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                "gatt",
                "tauri.ordered.forget-pending",
            ));
        }
        drop(locked);
        self.retire_ordered_resource(&key, &handle, group).await?;
        Ok(super::object([("state", super::string("forgotten"))]))
    }

    pub(super) async fn close_ordered_group(
        &self,
        caller: &crate::AuthenticatedCaller,
        payload: std::collections::BTreeMap<String, IpcValue>,
        ctl: ubm_desktop::OpControl,
    ) -> Result<IpcValue, super::DispatchError> {
        let group = self.ordered_group(caller, &payload).await?;
        self.close_ordered_resource(group, ctl).await
    }

    pub(super) async fn close_ordered_resource(
        &self,
        group: std::sync::Arc<tokio::sync::Mutex<OrderedGroup>>,
        ctl: ubm_desktop::OpControl,
    ) -> Result<IpcValue, super::DispatchError> {
        let authority = self.ensure_authority().await?;
        let mut group = group.lock().await;
        if !group.released {
            let outcome = authority
                .close_ordered_batch(&group.peer_id, group.collection_id, ctl)
                .await
                .map_err(|error| super::DispatchError::from_core(&error))?;
            let released = matches!(
                outcome,
                ubm_desktop::central::OrderedCleanupOutcome::Released
            );
            let status = native_status(authority.as_ref(), &group).await?;
            group.last_status = Some(status);
            group.released = released;
            let state = match outcome {
                ubm_desktop::central::OrderedCleanupOutcome::Released
                    if group.delivery.retained().is_some() =>
                {
                    "pending-drain"
                }
                ubm_desktop::central::OrderedCleanupOutcome::Released => "released",
                ubm_desktop::central::OrderedCleanupOutcome::PendingSource => "pending-source",
                ubm_desktop::central::OrderedCleanupOutcome::PendingDrain => "pending-drain",
            };
            return Ok(super::object([
                (
                    "nativeStatus",
                    super::string(group.last_status.clone().expect("stored status")),
                ),
                ("cleanupState", super::string(state)),
            ]));
        }
        let state = if group.delivery.retained().is_some() {
            "pending-drain"
        } else {
            "released"
        };
        Ok(super::object([
            (
                "nativeStatus",
                super::string(group.last_status.clone().expect("released status")),
            ),
            ("cleanupState", super::string(state)),
        ]))
    }
}

pub(super) fn json_value(value: serde_json::Value) -> IpcValue {
    match value {
        serde_json::Value::Null => IpcValue::Null,
        serde_json::Value::Bool(value) => IpcValue::Bool(value),
        serde_json::Value::Number(value) => IpcValue::Number(value),
        serde_json::Value::String(value) => IpcValue::String(value),
        serde_json::Value::Array(values) => {
            IpcValue::Array(values.into_iter().map(json_value).collect())
        }
        serde_json::Value::Object(values) => IpcValue::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, json_value(value)))
                .collect(),
        ),
    }
}

pub(super) async fn native_status(
    authority: &dyn super::CoreAuthority,
    group: &OrderedGroup,
) -> Result<String, super::DispatchError> {
    let members = authority
        .ordered_batch_members(&group.peer_id, group.collection_id)
        .await
        .map_err(|error| super::DispatchError::from_core(&error))?;
    let source = authority
        .ordered_batch_lease(&group.peer_id, group.collection_id)
        .await
        .map_err(|error| super::DispatchError::from_core(&error))?;
    let source_generation = source.map(|lease| match lease.source_generation {
        ubm_core::ordered_notifications::SourceGeneration::Counter(value) => {
            serde_json::json!({"kind":"counter", "value":value.to_string()})
        }
        ubm_core::ordered_notifications::SourceGeneration::Opaque(value) => {
            serde_json::json!({"kind":"opaque", "value":value})
        }
    });
    let (setup_error, fence_error) = authority
        .ordered_batch_errors(&group.peer_id, group.collection_id)
        .await
        .map_err(|error| super::DispatchError::from_core(&error))?;
    let members: Result<Vec<_>, super::DispatchError> = members.into_iter().map(|member| {
        let acquisition = match member.acquisition {
            ubm_desktop::central::OrderedMemberAcquisition::NotStarted => "not-started",
            ubm_desktop::central::OrderedMemberAcquisition::Acquired => "acquired",
            ubm_desktop::central::OrderedMemberAcquisition::Indeterminate => "indeterminate",
        };
        let delivery = match member.effective_delivery {
            ubm_desktop::ObservedDelivery::Notification => "notification",
            ubm_desktop::ObservedDelivery::Indication => "indication",
            ubm_desktop::ObservedDelivery::Unknown => "unknown",
        };
        Ok(serde_json::json!({"memberId":member.member_id.to_string(), "acquisition":acquisition,
            "effectiveDelivery":delivery, "cleanupError":member.cleanup_error.as_ref().map(normalized_error).transpose()?,
            "cleanupReleased":member.cleanup_released}))
    }).collect();
    Ok(serde_json::json!({"members":members?, "setupError":setup_error.as_ref().map(normalized_error).transpose()?,
        "fenceError":fence_error.as_ref().map(normalized_error).transpose()?, "sourceGeneration":source_generation}).to_string())
}

pub(super) fn normalized_error(
    error: &ubm_desktop::DesktopError,
) -> Result<serde_json::Value, super::DispatchError> {
    use ubm_core::ordered_notifications::{Poll, Record};
    let failure = ubm_desktop::ordered_notification_groups::failure_from_desktop(error);
    let mut observation = ubm_desktop::ordered_wire::encode_versioned(
        Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        }),
        2,
    )
    .ok_or_else(|| {
        super::DispatchError::new(
            ubm_core::contracts::BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.ordered.status-failure",
        )
    })?;
    let mut failure = observation
        .get_mut("failure")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| {
            super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                "ipc",
                "tauri.ordered.status-failure",
            )
        })?
        .clone();
    if let Some(metadata) = failure
        .get_mut("platformMetadata")
        .and_then(serde_json::Value::as_object_mut)
    {
        for value in metadata.values_mut() {
            let tagged = value.as_object_mut().ok_or_else(|| {
                super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.ordered.status-failure",
                )
            })?;
            let kind = tagged.remove("type").ok_or_else(|| {
                super::DispatchError::new(
                    ubm_core::contracts::BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.ordered.status-failure",
                )
            })?;
            tagged.insert("kind".into(), kind);
        }
    }
    let mut normalized = super::DispatchError::from_core(error)
        .normalized_error()
        .into_wire();
    normalized["orderedFailure"] = serde_json::Value::Object(failure);
    Ok(normalized)
}

async fn retained_response(
    authority: &dyn super::CoreAuthority,
    group: &mut OrderedGroup,
) -> Result<IpcValue, super::DispatchError> {
    if group.terminal_status_pending {
        group.terminal_status = Some(native_status(authority, group).await?);
        group.terminal_status_pending = false;
    }
    let (id, item) = group.delivery.retained().expect("retained response");
    let mut result = std::collections::BTreeMap::new();
    result.insert("state".into(), super::string("item"));
    result.insert("deliveryId".into(), super::string(id));
    result.insert("nativeWire".into(), item.clone());
    if let Some(status) = &group.terminal_status {
        result.insert("nativeStatus".into(), super::string(status.clone()));
    }
    Ok(IpcValue::Object(result))
}
