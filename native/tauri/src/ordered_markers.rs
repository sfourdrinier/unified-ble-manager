// native/tauri/src/ordered_markers.rs
//! Authorized access to centrally retained marker/write transactions.
use super::{BtleplugDispatcher, DispatchError};
use crate::desktop_core::{CoreAuthority, OrderedRecordingOwner};
use crate::{AuthenticatedCaller, IpcValue};
use std::{collections::BTreeMap, sync::Arc};
use ubm_core::contracts::BleErrorCode;
use ubm_desktop::ordered_recording::RecordingError;
use ubm_desktop::{DesktopError, PathSelector};

enum Owner {
    Group {
        authority: Arc<dyn CoreAuthority>,
        peer: String,
        id: u64,
        connection: String,
    },
    Recording {
        owner: Arc<dyn OrderedRecordingOwner>,
        peer: String,
    },
}
fn invalid() -> DispatchError {
    DispatchError::new(
        BleErrorCode::ProtocolMalformed,
        "ipc",
        "tauri.ordered.marker",
    )
}
fn recording_error(error: RecordingError) -> DispatchError {
    match error {
        RecordingError::Source(error) => DispatchError::from_core(&error),
        _ => DispatchError::new(
            BleErrorCode::LifecycleInvalidState,
            "gatt",
            "tauri.ordered.marker.owner",
        ),
    }
}
impl BtleplugDispatcher {
    pub(super) async fn ordered_marker_route(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
        binary: Option<Vec<u8>>,
        ctl: ubm_desktop::OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let key = super::caller_key(caller);
        let owner = if command.starts_with("gatt.ordered-recording.") {
            let handle =
                super::required_string(&payload, "startHandle", "tauri.ordered.marker.owner")?;
            let slot = self
                .ordered_recording_starts
                .lock()
                .await
                .get(&(key.clone(), handle.clone()))
                .cloned()
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered.marker.owner",
                    )
                })?;
            let slot = slot.state.lock().await;
            let expected_id = payload.get("__expectedLeaseId").and_then(super::as_string);
            let expected_generation = payload
                .get("__expectedLeaseGeneration")
                .and_then(super::as_string);
            let issued_session = expected_id == Some(slot.issuer_lease_id.as_str())
                && expected_generation == Some(slot.issuer_lease_generation.as_str());
            if !issued_session && !self.marker_reclaim_authorized(caller, &handle, &payload) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.ordered.marker.owner",
                ));
            }
            Owner::Recording {
                owner: slot.owner.clone().ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::LifecycleInvalidState,
                        "gatt",
                        "tauri.ordered.marker.owner",
                    )
                })?,
                peer: slot.peer.clone(),
            }
        } else {
            let group = self.ordered_group(caller, &payload).await?;
            let group = group.lock().await;
            Owner::Group {
                authority: self.ensure_authority().await?,
                peer: group.peer_id.clone(),
                id: group.collection_id,
                connection: group.connection_handle.clone(),
            }
        };
        let reservation = match &owner {
            Owner::Group {
                authority,
                peer,
                id,
                ..
            } => authority
                .group_marker_reservation(peer, *id)
                .await
                .map_err(|e| DispatchError::from_core(&e))?,
            Owner::Recording { owner, .. } => {
                owner.marker_reservation().await.map_err(recording_error)?
            }
        };
        let action = command.rsplit('.').next().ok_or_else(invalid)?;
        if action == "markers" {
            if binary.is_some() {
                return Err(invalid());
            }
            let markers = match reservation
                .active()
                .await
                .map_err(|e| DispatchError::from_core(&e))?
            {
                None => Vec::new(),
                Some((handle, status)) => vec![super::object([
                    ("markerHandle", super::string(handle.to_string())),
                    ("statusFormat", super::string("native")),
                    (
                        "status",
                        super::ordered_recordings::json_ipc(
                            ubm_desktop::ordered_wire::encode_marker_write(&status),
                        ),
                    ),
                ])],
            };
            return Ok(super::object([("markers", IpcValue::Array(markers))]));
        }
        if action == "reserve" {
            if binary.is_some() {
                return Err(invalid());
            }
            let token = super::required_string(
                &payload,
                "reservationToken",
                "tauri.ordered.marker.reserve",
            )?;
            let handle = reservation
                .reserve(&token)
                .await
                .map_err(|e| DispatchError::from_core(&e))?;
            return Ok(super::object([(
                "markerHandle",
                super::string(handle.to_string()),
            )]));
        }
        let raw = super::required_string(&payload, "markerHandle", "tauri.ordered.marker.handle")?;
        let id: u64 = raw.parse().map_err(|_| invalid())?;
        if id.to_string() != raw {
            return Err(invalid());
        }
        if action == "forget" {
            if binary.is_some() {
                return Err(invalid());
            }
            reservation
                .forget(id)
                .await
                .map_err(|e| DispatchError::from_core(&e))?;
            return Ok(super::object([("forgotten", IpcValue::Bool(true))]));
        }
        let status = if action == "start" {
            let bytes = binary.ok_or_else(invalid)?;
            let with_response = match payload.get("withResponse") {
                Some(IpcValue::Bool(v)) => *v,
                _ => return Err(invalid()),
            };
            let selector = self.marker_selector(&key, &owner, &payload).await?;
            if ctl.ticket.is_cancel_requested() {
                reservation.cancel(id).await
            } else {
                tokio::select! {
                    result = reservation.start(id, selector, bytes, with_response) => {
                        if ctl.ticket.is_cancel_requested() { reservation.cancel(id).await } else { result }
                    }
                    _ = ctl.ticket.cancelled() => reservation.cancel(id).await,
                }
            }
        } else if binary.is_some() {
            return Err(invalid());
        } else if action == "status" {
            reservation.status(id).await
        } else if action == "cancel" {
            reservation.cancel(id).await
        } else {
            return Err(invalid());
        }
        .map_err(|e| DispatchError::from_core(&e))?;
        Ok(super::object([
            (
                "status",
                super::ordered_recordings::json_ipc(
                    ubm_desktop::ordered_wire::encode_marker_write(&status),
                ),
            ),
            ("statusFormat", super::string("native")),
        ]))
    }
    async fn marker_selector(
        &self,
        key: &str,
        marker_owner: &Owner,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<PathSelector, DispatchError> {
        let db_handle =
            super::required_string(payload, "databaseHandle", "tauri.ordered.marker.database")?;
        let characteristic = super::required_string(
            payload,
            "characteristicHandle",
            "tauri.ordered.marker.target",
        )?;
        let selector = {
            let state = self.inner.lock().await;
            let owner = state
                .callers
                .get(key)
                .filter(|o| !o.retired)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered.marker.owner",
                    )
                })?;
            let db = owner
                .databases
                .get(&db_handle)
                .filter(|d| d.valid)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "gatt",
                        "tauri.ordered.marker.database",
                    )
                })?;
            super::validate_database_identity(payload, db, "tauri.ordered.marker.database")?;
            let connection = owner
                .connections
                .get(&db.connection_handle)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::ConnectionStale,
                        "connection",
                        "tauri.ordered.marker.connection",
                    )
                })?;
            super::validate_connection_identity(
                payload,
                connection,
                &owner.lease_id,
                "tauri.ordered.marker.connection",
            )?;
            match marker_owner {
                Owner::Group {
                    peer,
                    connection: expected,
                    ..
                } if connection.peer_id == *peer && db.connection_handle == *expected => {}
                Owner::Recording { peer, .. } if connection.peer_id == *peer => {}
                _ => {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.ordered.marker.connection",
                    ))
                }
            }
            db.characteristics
                .get(&characteristic)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattNotFound,
                        "gatt",
                        "tauri.ordered.marker.target",
                    )
                })?
                .selector
                .clone()
        };
        self.ensure_authority()
            .await?
            .ordered_selector(&selector)
            .map_err(|e: DesktopError| DispatchError::from_core(&e))
    }
}
