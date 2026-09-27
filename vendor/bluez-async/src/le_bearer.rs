//! Explicit, daemon-epoch-pinned LE lifecycle. Never falls back to Device1.

use crate::{BluetoothError, BluetoothSession, DeviceId, MatchFailure, match_error, match_failure};
use dbus::nonblock::Proxy;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const LE: &str = "org.bluez.Bearer.LE1";
const CONNECTION_CONFIRMATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
type Reply = Shared<BoxFuture<'static, Result<(), MatchFailure>>>;

#[derive(Default)]
pub(crate) struct Entry {
    operation: tokio::sync::Mutex<()>,
    pending: Mutex<Option<Reply>>,
    release: Mutex<Option<Reply>>,
}

/// Accepted Connect replies outlive a caller's cancelled wait. A subsequent
/// cleanup drives the same reply, rather than mistaking NotConnected for
/// cancellation of a still-pending native acquisition.
#[derive(Default)]
pub(crate) struct Registry(Mutex<HashMap<(String, DeviceId), Arc<Entry>>>);

struct EntryLease {
    registry: Arc<Registry>,
    key: (String, DeviceId),
    entry: Arc<Entry>,
}

impl std::ops::Deref for EntryLease {
    type Target = Arc<Entry>;
    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl Drop for EntryLease {
    fn drop(&mut self) {
        let mut entries = self.registry.0.lock().unwrap();
        if Arc::strong_count(&self.entry) == 2
            && self.entry.pending.lock().unwrap().is_none()
            && self.entry.release.lock().unwrap().is_none()
            && entries
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            entries.remove(&self.key);
        }
    }
}

fn failure(message: &str) -> BluetoothError {
    dbus::Error::new_custom("org.bluez.Error.NotSupported", message).into()
}

fn indeterminate(error: &MatchFailure) -> bool {
    matches!(
        error.name.as_str(),
        "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.TimedOut"
    )
}

fn absent_method(error: &MatchFailure) -> bool {
    matches!(
        error.name.as_str(),
        "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface"
    )
}

impl BluetoothSession {
    /// Pin all subsequent GATT/object calls and event matches to the same
    /// attested daemon process. This does not open a second bus connection.
    pub async fn with_le_owner(&self, owner: &str) -> Result<Self, BluetoothError> {
        self.le_owner(owner).await?;
        let mut session = self.clone();
        session.destination = owner.to_owned();
        Ok(session)
    }

    async fn le_owner_present(&self, owner: &str) -> Result<bool, BluetoothError> {
        dbus::strings::BusName::new(owner)
            .map_err(|_| failure("LE bearer attestation requires a valid D-Bus unique owner"))?;
        if !owner.starts_with(':') {
            return Err(failure("LE bearer requires a unique owner"));
        }
        let bus = Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            crate::DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let (present,): (bool,) = bus
            .method_call("org.freedesktop.DBus", "NameHasOwner", (owner,))
            .await?;
        Ok(present)
    }

    async fn le_owner(&self, owner: &str) -> Result<(), BluetoothError> {
        dbus::strings::BusName::new(owner)
            .map_err(|_| failure("LE bearer attestation requires a valid D-Bus unique owner"))?;
        if !owner.starts_with(':') {
            return Err(failure(
                "LE bearer attestation requires a unique, not well-known, owner",
            ));
        }
        let bus = Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            crate::DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let (current,): (String,) = bus
            .method_call("org.freedesktop.DBus", "GetNameOwner", ("org.bluez",))
            .await?;
        if current != owner {
            return Err(failure(
                "the attested BlueZ daemon owner changed; a new host attestation is required",
            ));
        }
        Ok(())
    }

    fn le_entry(&self, id: &DeviceId, owner: &str) -> EntryLease {
        let key = (owner.to_owned(), id.clone());
        let entry = self
            .le
            .0
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        EntryLease {
            registry: self.le.clone(),
            key,
            entry,
        }
    }

    fn retire_le_entry(&self, id: &DeviceId, owner: &str, entry: &Arc<Entry>) {
        let mut entries = self.le.0.lock().unwrap();
        let key = (owner.to_owned(), id.clone());
        // A queued user already holds this same entry/operation lock. Never
        // replace its lock with a fresh one. Without another user, removal is
        // atomic with admission's map lookup and prevents lifetime growth.
        if entries
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
            && Arc::strong_count(entry) == 2
        {
            entries.remove(&key);
        }
    }

    /// Read the LE bearer, never Device1's aggregate Connected property.
    pub async fn le_connected(&self, id: &DeviceId, owner: &str) -> Result<bool, BluetoothError> {
        self.le_owner(owner).await?;
        let proxy = Proxy::new(
            owner,
            id.object_path.clone(),
            crate::DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let connected = proxy.get(LE, "Connected").await?;
        self.le_owner(owner).await?;
        Ok(connected)
    }

    /// Connect only the attested daemon's LE bearer. This reports a link,
    /// not GATT readiness; Device1.ServicesResolved is not LE-specific.
    pub async fn connect_le(&self, id: &DeviceId, owner: &str) -> Result<(), BluetoothError> {
        self.le_owner(owner).await?;
        let entry = self.le_entry(id, owner);
        let _operation = entry.operation.lock().await;
        self.le_owner(owner).await?;
        if entry.release.lock().unwrap().is_some() {
            self.finish_le_release(id, owner, &entry).await?;
        }
        let existing = entry.pending.lock().unwrap().clone();
        if let Some(pending) = existing {
            pending.await.map_err(match_error)?;
            if self.le_connected(id, owner).await? {
                return Ok(());
            }
        }
        let connection = self.connection.clone();
        self.le_owner(owner).await?;
        let destination = owner.to_owned();
        let path = id.object_path.clone();
        let pending = async move {
            let proxy = Proxy::new(
                destination,
                path,
                crate::DBUS_METHOD_CALL_TIMEOUT,
                connection,
            );
            match proxy.method_call::<(), _, _, _>(LE, "Connect", ()).await {
                Ok(()) => Ok(()),
                Err(error) if crate::connect_already_connected_is_success(error.name()) => Ok(()),
                Err(error) => Err(match_failure(error)),
            }
        }
        .boxed()
        .shared();
        *entry.pending.lock().unwrap() = Some(pending.clone());
        pending.await.map_err(match_error)?;
        // The reply can precede the daemon's property update. Poll only this
        // bearer; a timeout remains owned cleanup debt, not a fake release.
        tokio::time::timeout(CONNECTION_CONFIRMATION_TIMEOUT, async {
            loop {
                if self.le_connected(id, owner).await? {
                    return Ok(());
                }
                tokio::time::sleep(crate::DISCONNECT_CONFIRMATION_POLL).await;
            }
        })
        .await
        .unwrap_or(Err(BluetoothError::LeConnectionNotConfirmed))
    }

    /// Settle accepted acquisition before asking this exact LE bearer to end.
    /// Refused or indeterminate cleanup leaves the original entry retryable.
    pub async fn disconnect_le(&self, id: &DeviceId, owner: &str) -> Result<(), BluetoothError> {
        let Some(entry) = self
            .le
            .0
            .lock()
            .unwrap()
            .get(&(owner.to_owned(), id.clone()))
            .cloned()
        else {
            // This session never admitted native LE acquisition for this
            // identity. There is no owned work to compensate; do not issue a
            // disconnect against somebody else's connection.
            return Ok(());
        };
        let entry = EntryLease {
            registry: self.le.clone(),
            key: (owner.to_owned(), id.clone()),
            entry,
        };
        let _operation = entry.operation.lock().await;
        let existing_release = entry.release.lock().unwrap().clone();
        if let Some(release) = existing_release {
            if let Some(Err(error)) = release.peek() {
                if indeterminate(error) {
                    return Err(match_error(error.clone()));
                }
            }
            if !matches!(release.peek(), Some(Err(_))) {
                self.finish_le_release(id, owner, &entry).await?;
                self.retire_le_entry(id, owner, &entry);
                return Ok(());
            }
            // Only a settled refusal is retryable. A still-pending release
            // keeps its exact reply and cannot race a second native request.
        }
        let pending = entry.pending.lock().unwrap().clone();
        if pending
            .as_ref()
            .and_then(Reply::peek)
            .is_some_and(|result| result.as_ref().is_err_and(absent_method))
        {
            *entry.pending.lock().unwrap() = None;
            self.retire_le_entry(id, owner, &entry);
            return Ok(());
        }
        if pending.is_none() {
            self.retire_le_entry(id, owner, &entry);
            return Ok(());
        }
        if !self.le_owner_present(owner).await? {
            return Err(failure(
                "the attested daemon disappeared before LE release was confirmed; physical link state is unknown",
            ));
        }
        self.le_owner(owner).await?;
        if let Some(pending) = pending {
            // An indeterminate method timeout is not evidence that native
            // work stopped. Preserve it rather than accepting NotConnected.
            if let Err(error) = pending.await {
                if indeterminate(&error) {
                    return Err(match_error(error));
                }
                if absent_method(&error) {
                    *entry.pending.lock().unwrap() = None;
                    self.retire_le_entry(id, owner, &entry);
                    return Ok(());
                }
                // An actual refusal has settled. Still inspect the LE bearer:
                // a failed acquisition can have partially established a link.
                if !self.le_connected(id, owner).await? {
                    *entry.pending.lock().unwrap() = None;
                    self.retire_le_entry(id, owner, &entry);
                    return Ok(());
                }
            }
        }
        self.le_owner(owner).await?;
        let connection = self.connection.clone();
        let destination = owner.to_owned();
        let path = id.object_path.clone();
        let release = async move {
            let proxy = Proxy::new(
                destination,
                path,
                crate::DBUS_METHOD_CALL_TIMEOUT,
                connection,
            );
            match proxy.method_call::<(), _, _, _>(LE, "Disconnect", ()).await {
                Ok(()) => Ok(()),
                Err(error) if error.name() == Some("org.bluez.Error.NotConnected") => Ok(()),
                Err(error) => Err(match_failure(error)),
            }
        }
        .boxed()
        .shared();
        *entry.release.lock().unwrap() = Some(release);
        self.finish_le_release(id, owner, &entry).await?;
        self.retire_le_entry(id, owner, &entry);
        Ok(())
    }

    async fn finish_le_release(
        &self,
        id: &DeviceId,
        owner: &str,
        entry: &Entry,
    ) -> Result<(), BluetoothError> {
        let release = entry
            .release
            .lock()
            .unwrap()
            .clone()
            .expect("release admitted under operation lock");
        release.await.map_err(match_error)?;
        tokio::time::timeout(crate::DISCONNECT_CONFIRMATION_TIMEOUT, async {
            loop {
                if !self.le_connected(id, owner).await? {
                    return Ok(());
                }
                tokio::time::sleep(crate::DISCONNECT_CONFIRMATION_POLL).await;
            }
        })
        .await
        .unwrap_or(Err(BluetoothError::DisconnectConfirmationTimedOut))?;
        *entry.pending.lock().unwrap() = None;
        *entry.release.lock().unwrap() = None;
        Ok(())
    }
}
