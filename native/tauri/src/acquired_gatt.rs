//! IPC mappings contain ownership only; FD acquisition and I/O stay native.
use super::*;

#[derive(Clone)]
pub(super) struct CoreAcquiredWriter {
    pub(super) peer_id: String,
    pub(super) connection_handle: String,
    pub(super) database_handle: String,
    native_handle: String,
    native_lease: String,
    phase: ReleasePhase,
}

impl BtleplugDispatcher {
    pub(super) async fn release_acquired_database(
        &self,
        key: &str,
        database: &str,
        reason: &str,
    ) -> Result<(), DispatchError> {
        let (writers, notifications, lease) = {
            let state = self.inner.lock().await;
            let owner = state.callers.get(key).ok_or_else(acquired_owner_error)?;
            (
                owner
                    .acquired_writers
                    .iter()
                    .filter_map(|(handle, writer)| {
                        (writer.database_handle == database).then_some(handle.clone())
                    })
                    .collect::<Vec<_>>(),
                owner
                    .subscriptions
                    .iter()
                    .filter_map(|(handle, subscription)| {
                        (subscription.database_handle == database
                            && subscription.acquired.is_some())
                        .then_some(handle.clone())
                    })
                    .collect::<Vec<_>>(),
                (owner.lease_id.clone(), owner.lease_generation.clone()),
            )
        };
        for handle in writers {
            self.release_acquired_writer(key, &handle, OpControl::unbounded())
                .await?;
        }
        for handle in notifications {
            self.notification_terminal(key, (&lease.0, &lease.1), &handle, reason, None)
                .await?;
            self.release_subscription(key, &handle, OpControl::unbounded())
                .await?;
        }
        Ok(())
    }

    pub(super) async fn acquire_writer(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let target = self.gatt_target(caller, &payload).await?;
        let database_handle =
            required_string(&payload, "databaseHandle", "tauri.acquire-write.database")?;
        let expected = expected_lease(&payload, "tauri.acquire-write.lease")?;
        let key = caller_key(caller);
        let authority = self.ensure_authority().await?;
        let native = authority
            .acquire_gatt(
                &target.peer_id,
                &target.characteristic.selector,
                ubm_desktop::acquired_gatt::AcquisitionKind::Write,
                ctl.with_connection_lease(target.lease.clone()),
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let handle = self.id("acquired-writer");
        let published = {
            let mut state = self.inner.lock().await;
            match state
                .callers
                .get_mut(&key)
                .filter(|owner| !owner.retired && lease_matches(owner, &expected))
            {
                Some(owner)
                    if owner
                        .connections
                        .get(&target.connection_handle)
                        .is_some_and(|connection| connection.phase.is_active())
                        && owner
                            .databases
                            .get(&database_handle)
                            .is_some_and(|database| database.valid) =>
                {
                    owner.acquired_writers.insert(
                        handle.clone(),
                        CoreAcquiredWriter {
                            peer_id: target.peer_id,
                            connection_handle: target.connection_handle,
                            database_handle,
                            native_handle: native.handle.clone(),
                            native_lease: target.lease.clone(),
                            phase: ReleasePhase::Active,
                        },
                    );
                    true
                }
                _ => false,
            }
        };
        if !published {
            self.compensate(
                &authority,
                &key,
                OrphanResource::Acquired {
                    handle: native.handle,
                    lease: target.lease,
                },
            )
            .await;
            return Err(DispatchError::new(
                BleErrorCode::ConnectionStale,
                "gatt",
                "tauri.acquire-write.publication",
            ));
        }
        Ok(object([
            ("handle", string(handle)),
            ("mtuBytes", number(i64::from(native.mtu))),
        ]))
    }

    pub(super) async fn acquired_writer(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<CoreAcquiredWriter, DispatchError> {
        let connection = self
            .connection(caller, payload, "tauri.acquired-write.connection")
            .await?;
        let handle = required_string(payload, "acquiredHandle", "tauri.acquired-write.handle")?;
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.acquired-write.database")?;
        let state = self.inner.lock().await;
        let owner = state
            .callers
            .get(&caller_key(caller))
            .ok_or_else(acquired_owner_error)?;
        let writer = owner
            .acquired_writers
            .get(&handle)
            .ok_or_else(acquired_owner_error)?;
        if writer.native_lease != connection.lease
            || writer.peer_id != connection.peer_id
            || writer.database_handle != database_handle
            || !writer.phase.is_active()
        {
            return Err(acquired_owner_error());
        }
        let database = owner
            .databases
            .get(&database_handle)
            .ok_or_else(acquired_owner_error)?;
        if !database.valid
            || database.database_id
                != required_string(payload, "databaseId", "tauri.acquired-write.database-id")?
            || database.database_generation
                != required_string(
                    payload,
                    "databaseGeneration",
                    "tauri.acquired-write.generation",
                )?
        {
            return Err(DispatchError::new(
                BleErrorCode::GattStaleHandle,
                "gatt",
                "tauri.acquired-write.generation",
            ));
        }
        Ok(writer.clone())
    }

    pub(super) async fn write_acquired(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let writer = self.acquired_writer(caller, &payload).await?;
        let value = bytes.ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::BytesInvalid,
                "gatt",
                "tauri.acquired-write.bytes",
            )
        })?;
        let size = value.len();
        self.ensure_authority()
            .await?
            .acquired_write(
                &writer.native_handle,
                value,
                ctl.with_connection_lease(writer.native_lease),
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([
            (
                "terminal",
                object([
                    ("correlation", string(self.id("acquired-write-operation"))),
                    ("outcome", string("succeeded")),
                    ("cause", IpcValue::Null),
                ]),
            ),
            ("mode", string("without-response")),
            ("commitState", string("unknown")),
            ("bytesSubmitted", number(size as i64)),
        ]))
    }

    pub(super) async fn close_acquired_writer(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "acquiredHandle", "tauri.acquired-write.close")?;
        self.release_acquired_writer(&caller_key(caller), &handle, ctl)
            .await?;
        Ok(released())
    }

    pub(super) async fn release_acquired_writer(
        &self,
        key: &str,
        handle: &str,
        ctl: OpControl,
    ) -> Result<(), DispatchError> {
        let step = {
            let mut state = self.inner.lock().await;
            let owner = state
                .callers
                .get_mut(key)
                .ok_or_else(acquired_owner_error)?;
            if owner.acquired_releases.contains(handle) {
                return Ok(());
            }
            let writer = owner
                .acquired_writers
                .get_mut(handle)
                .ok_or_else(acquired_owner_error)?;
            (
                begin_release(&mut writer.phase),
                writer.native_handle.clone(),
                writer.native_lease.clone(),
            )
        };
        let (sender, native, lease) = match step {
            (ReleaseStep::Join(answer), ..) => return join_release(answer).await,
            (ReleaseStep::Lead(sender), native, lease) => (sender, native, lease),
        };
        let result = match self.ensure_authority().await {
            Ok(authority) => authority
                .close_acquired(&native, ctl.with_connection_lease(lease))
                .await
                .map_err(|error| DispatchError::from_core(&error)),
            Err(error) => Err(error),
        };
        {
            let mut state = self.inner.lock().await;
            if let Some(owner) = state.callers.get_mut(key) {
                if result.is_ok() {
                    owner.acquired_writers.remove(handle);
                    owner.acquired_releases.insert(handle.to_owned());
                } else if let Some(writer) = owner.acquired_writers.get_mut(handle) {
                    writer.phase = ReleasePhase::ReleaseFailed;
                }
            }
        }
        let _ = sender.send(Some(result.clone()));
        result
    }
}

fn acquired_owner_error() -> DispatchError {
    DispatchError::new(
        BleErrorCode::OwnershipDenied,
        "gatt",
        "tauri.acquired-write.owner",
    )
}
