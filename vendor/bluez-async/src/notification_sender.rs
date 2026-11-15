// vendor/bluez-async/src/notification_sender.rs
//! Dedicated D-Bus sender for one BlueZ StartNotify ownership.

use crate::{BluetoothError, BluetoothSession, CharacteristicId};
use dbus::channel::BusType;
use dbus::nonblock::{Proxy, SyncConnection};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationSenderFailure {
    pub domain: &'static str,
    pub code: String,
    pub message: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationSenderTerminal {
    Failed(NotificationSenderFailure),
    Closed,
}
#[derive(Default)]
struct ResourceState {
    terminal: Mutex<Option<NotificationSenderTerminal>>,
    notify: Notify,
}
#[derive(Clone)]
pub struct NotificationSenderMonitor(Arc<ResourceState>);
impl NotificationSenderMonitor {
    pub async fn terminal(&self) -> NotificationSenderTerminal {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(terminal) = self
                .0
                .terminal
                .lock()
                .expect("notification sender state poisoned")
                .clone()
            {
                return terminal;
            }
            notified.await;
        }
    }
    fn finish(&self, terminal: NotificationSenderTerminal) {
        let mut state = self
            .0
            .terminal
            .lock()
            .expect("notification sender state poisoned");
        if state.is_none() {
            *state = Some(terminal);
        }
        drop(state);
        self.0.notify.notify_waiters();
    }
}

/// Closing the private sender retires only its own BlueZ notify registration.
/// A successful close is a daemon-side logical registration barrier, not proof
/// that the device acknowledged a physical CCCD write.
pub struct NotificationSender {
    observer: Arc<SyncConnection>,
    connection: Option<Arc<SyncConnection>>,
    resource: Option<JoinHandle<()>>,
    sender: String,
    daemon: String,
    monitor: NotificationSenderMonitor,
}
impl Drop for NotificationSender {
    fn drop(&mut self) {
        self.monitor.finish(NotificationSenderTerminal::Closed);
        if let Some(resource) = self.resource.take() {
            resource.abort();
        }
        self.connection.take();
    }
}
async fn bus_id(connection: Arc<SyncConnection>) -> Result<String, dbus::Error> {
    let bus = Proxy::new(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        TIMEOUT,
        connection,
    );
    let (id,): (String,) = bus.method_call("org.freedesktop.DBus", "GetId", ()).await?;
    Ok(id)
}
impl BluetoothSession {
    /// Return the currently observed unique BlueZ owner, if initialized.
    pub fn notification_daemon_owner(&self) -> Option<String> {
        self.peer_observers.current_owner()
    }

    /// Open a private sender on the same D-Bus instance as this session.
    pub async fn notification_sender(
        &self,
        daemon: &str,
    ) -> Result<NotificationSender, BluetoothError> {
        if self.peer_observers.current_owner().as_deref() != Some(daemon) {
            return Err(dbus::Error::new_custom(
                "org.unifiedblemanager.Error.StaleOwner",
                "BlueZ daemon ownership changed",
            )
            .into());
        }
        let bus = self.notification_bus.ok_or_else(|| {
            dbus::Error::new_custom(
                "org.unifiedblemanager.Error.UnknownBus",
                "The session bus identity is unavailable",
            )
        })?;
        let (resource, connection) = match bus {
            BusType::Session => dbus_tokio::connection::new_session_sync()?,
            BusType::System => dbus_tokio::connection::new_system_sync()?,
            _ => {
                return Err(dbus::Error::new_custom(
                    "org.unifiedblemanager.Error.UnknownBus",
                    "Unsupported notification bus",
                )
                .into());
            }
        };
        let sender = connection.unique_name().to_string();
        let monitor = NotificationSenderMonitor(Arc::new(ResourceState::default()));
        let resource_monitor = monitor.clone();
        let task = tokio::spawn(async move {
            let error = resource.await;
            let failure = match error {
                dbus_tokio::connection::IOResourceError::Dbus(error) => NotificationSenderFailure {
                    domain: "bluez-dbus",
                    code: error
                        .name()
                        .unwrap_or("org.freedesktop.DBus.Error.Failed")
                        .to_owned(),
                    message: error
                        .message()
                        .unwrap_or("D-Bus notification sender failed")
                        .to_owned(),
                },
                dbus_tokio::connection::IOResourceError::Io(error) => NotificationSenderFailure {
                    domain: "bluez-io",
                    code: error
                        .raw_os_error()
                        .map_or_else(|| format!("{:?}", error.kind()), |code| code.to_string()),
                    message: error.to_string(),
                },
                _ => NotificationSenderFailure {
                    domain: "bluez-io",
                    code: "unknown".into(),
                    message: "Notification sender reactor failed".into(),
                },
            };
            resource_monitor.finish(NotificationSenderTerminal::Failed(failure));
        });
        let result = NotificationSender {
            observer: self.connection.clone(),
            connection: Some(connection),
            resource: Some(task),
            sender,
            daemon: daemon.to_owned(),
            monitor,
        };
        let original_id = bus_id(result.observer.clone()).await?;
        let private_id = bus_id(result.connection.as_ref().expect("open sender").clone()).await?;
        if original_id != private_id
            || self.peer_observers.current_owner().as_deref() != Some(daemon)
        {
            return Err(dbus::Error::new_custom(
                "org.unifiedblemanager.Error.StaleOwner",
                "D-Bus or BlueZ ownership changed",
            )
            .into());
        }
        Ok(result)
    }
}
impl NotificationSender {
    pub fn monitor(&self) -> NotificationSenderMonitor {
        self.monitor.clone()
    }

    /// Issue StartNotify on the pinned BlueZ daemon and exact characteristic path.
    /// Callers must retain an in-flight operation independently of cancellation.
    pub async fn start(&self, characteristic: &CharacteristicId) -> Result<(), BluetoothError> {
        let connection = self.connection.as_ref().ok_or_else(|| {
            dbus::Error::new_custom(
                "org.unifiedblemanager.Error.Closed",
                "Notification sender is closed",
            )
        })?;
        let path: dbus::Path<'static> = characteristic.clone().into();
        let proxy = Proxy::new(
            self.daemon.clone(),
            path,
            Duration::from_secs(i32::MAX as u64),
            connection.clone(),
        );
        proxy
            .method_call::<(), _, _, _>("org.bluez.GattCharacteristic1", "StartNotify", ())
            .await?;
        Ok(())
    }

    /// Process previously queued daemon messages on the parent connection.
    pub async fn synchronize(&self) -> Result<(), BluetoothError> {
        let peer = Proxy::new(self.daemon.clone(), "/", TIMEOUT, self.observer.clone());
        peer.method_call::<(), _, _, _>("org.freedesktop.DBus.Peer", "Ping", ())
            .await?;
        Ok(())
    }

    /// Close the unique sender, then observe its loss and a response from the
    /// original daemon. Retry on any error; the sender name remains retained.
    pub async fn close(&mut self) -> Result<(), BluetoothError> {
        self.monitor.finish(NotificationSenderTerminal::Closed);
        if let Some(resource) = self.resource.take() {
            resource.abort();
            let _ = resource.await;
        }
        self.connection.take();
        let bus = Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            TIMEOUT,
            self.observer.clone(),
        );
        let absent = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (present,): (bool,) = bus
                    .method_call("org.freedesktop.DBus", "NameHasOwner", (&self.sender,))
                    .await?;
                if !present {
                    return Ok::<(), dbus::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .map_err(|_| {
            dbus::Error::new_custom(
                "org.unifiedblemanager.Error.CleanupPending",
                "Notification sender absence was not observed",
            )
        })?;
        absent?;
        let (daemon_present,): (bool,) = bus
            .method_call("org.freedesktop.DBus", "NameHasOwner", (&self.daemon,))
            .await?;
        if daemon_present {
            let peer = Proxy::new(self.daemon.clone(), "/", TIMEOUT, self.observer.clone());
            peer.method_call::<(), _, _, _>("org.freedesktop.DBus.Peer", "Ping", ())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sender_terminal_is_retained_and_wakes_waiters() {
        let monitor = NotificationSenderMonitor(Arc::new(ResourceState::default()));
        let other = monitor.clone();
        let waiter = tokio::spawn(async move { other.terminal().await });
        let failure = NotificationSenderFailure {
            domain: "bluez-io",
            code: "fixture".into(),
            message: "fixture".into(),
        };
        monitor.finish(NotificationSenderTerminal::Failed(failure.clone()));
        monitor.finish(NotificationSenderTerminal::Closed);
        assert_eq!(
            waiter.await.unwrap(),
            NotificationSenderTerminal::Failed(failure.clone())
        );
        assert_eq!(
            monitor.terminal().await,
            NotificationSenderTerminal::Failed(failure)
        );
    }
}
