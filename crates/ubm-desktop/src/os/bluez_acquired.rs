//! One D-Bus sender owns each acquired FD, including a deferred acquisition.
//! Sender death cancels daemon-side admission when the opening future ends.
//! Failed cleanup remains in the process vault and is retried by its parent.
use crate::{
    acquired_gatt::{AcquiredGattIo, LinuxAcquiredGattIo, TransportFuture},
    errors::DesktopError,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

static NEXT: AtomicU64 = AtomicU64::new(1);
static OWNERS: OnceLock<Mutex<HashMap<u64, Arc<SenderOwner>>>> = OnceLock::new();
fn owners() -> &'static Mutex<HashMap<u64, Arc<SenderOwner>>> {
    OWNERS.get_or_init(Mutex::default)
}
fn failure(code: BleErrorCode, detail: impl Into<String>) -> DesktopError {
    DesktopError::new(code, BleErrorDomain::Cleanup, "gatt.acquired.close").with_detail(detail)
}

async fn sender_absent(observer: &zbus::Connection, sender: &str) -> Result<(), DesktopError> {
    // This bounded cleanup backstop is also interrupted by the parent's
    // original operation budget. Dropping it retains the exact vault row.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let present: bool = observer
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(sender,),
                )
                .await
                .and_then(|reply| reply.body().deserialize())
                .map_err(|error| {
                    failure(BleErrorCode::PlatformFailure, error.to_string())
                        .with_platform(super::bluez_dbus_detail(&error))
                })?;
            if !present {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .map_err(|_| {
        failure(
            BleErrorCode::OperationTimedOut,
            "acquired sender termination is not yet observed; cleanup remains owned",
        )
    })?
}

#[derive(Debug)]
pub(super) struct SenderOwner {
    id: u64,
    parent: String,
    terminal: AtomicBool,
    connection: Mutex<Option<zbus::Connection>>,
    observer: Mutex<Option<zbus::Connection>>,
    sender_name: Mutex<Option<String>>,
    fd: Mutex<Option<Arc<LinuxAcquiredGattIo>>>,
    close_gate: tokio::sync::Mutex<()>,
}

pub(super) struct OpeningOwner {
    owner: Arc<SenderOwner>,
    published: bool,
}
impl OpeningOwner {
    pub(super) fn reserve(parent: String) -> Result<Self, DesktopError> {
        let mut rows = owners()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rows.len() >= 4096 || rows.values().filter(|row| row.parent == parent).count() >= 256 {
            return Err(failure(
                BleErrorCode::StreamQuota,
                "acquired sender capacity includes pending openings and retained cleanup",
            ));
        }
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| {
                failure(
                    BleErrorCode::StreamQuota,
                    "acquired sender identity exhausted",
                )
            })?;
        let owner = Arc::new(SenderOwner {
            id,
            parent,
            terminal: AtomicBool::new(false),
            connection: Mutex::new(None),
            observer: Mutex::new(None),
            sender_name: Mutex::new(None),
            fd: Mutex::new(None),
            close_gate: tokio::sync::Mutex::new(()),
        });
        rows.insert(id, owner.clone());
        Ok(Self {
            owner,
            published: false,
        })
    }
    pub(super) fn install_connection(&self, connection: zbus::Connection) {
        *self
            .owner
            .sender_name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            connection.unique_name().map(ToString::to_string);
        *self
            .owner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection);
    }
    pub(super) fn observe_with(&self, observer: zbus::Connection) {
        *self
            .owner
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
    }
    pub(super) fn install_fd(&self, fd: Arc<LinuxAcquiredGattIo>) {
        *self
            .owner
            .fd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fd);
    }
    pub(super) async fn close(&self) -> Result<(), DesktopError> {
        self.owner.close().await
    }
    pub(super) fn publish(mut self) -> Arc<dyn AcquiredGattIo> {
        self.published = true;
        self.owner.clone()
    }
}
impl Drop for OpeningOwner {
    fn drop(&mut self) {
        if !self.published {
            self.owner.terminal.store(true, Ordering::Release);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let owner = self.owner.clone();
                runtime.spawn(async move {
                    if let Err(error) = owner.close().await {
                        eprintln!("ubm-desktop: acquired sender cleanup retained: {error}");
                    }
                });
            }
            // Without an executor the strong process-vault row remains owned.
        }
    }
}
impl SenderOwner {
    fn fd(&self) -> Result<Arc<LinuxAcquiredGattIo>, DesktopError> {
        if self.terminal.load(Ordering::Acquire) {
            return Err(failure(
                BleErrorCode::StreamClosed,
                "acquired transport is closing",
            ));
        }
        self.fd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                failure(
                    BleErrorCode::StreamClosed,
                    "acquired transport has no published descriptor",
                )
            })
    }
}
impl AcquiredGattIo for SenderOwner {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> TransportFuture<'a, ()> {
        Box::pin(async move { self.fd()?.send(bytes).await })
    }
    fn receive(&self) -> TransportFuture<'_, Vec<u8>> {
        Box::pin(async move { self.fd()?.receive().await })
    }
    fn close(&self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            self.terminal.store(true, Ordering::Release);
            let _gate = self.close_gate.lock().await;
            let fd = self
                .fd
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut failures = Vec::new();
            if let Some(fd) = fd {
                match fd.close().await {
                    Ok(()) => {
                        self.fd
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                    }
                    Err(error) => failures.push(error),
                }
            }
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(connection) = connection {
                if !connection.is_closed()
                    && let Err(error) = connection.close().await
                {
                    failures.push(
                        failure(BleErrorCode::PlatformFailure, error.to_string())
                            .with_platform(super::bluez_dbus_detail(&error)),
                    );
                }
                // zbus marks the connection closed even after a failed
                // shutdown. Drop its owned reference and observe sender death
                // on the independent parent before retiring this obligation.
                self.connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            }
            let sender = self
                .sender_name
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(sender) = sender {
                let observer = self
                    .observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let observed = match observer {
                    Some(observer) => sender_absent(&observer, &sender).await,
                    None => {
                        return Err(failure(
                            BleErrorCode::LifecycleInvariantViolation,
                            "acquired sender has no independent cleanup observer",
                        ));
                    }
                };
                match observed {
                    Ok(()) => {
                        self.sender_name
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                    }
                    Err(error) => failures.push(error),
                }
            }
            if failures.is_empty() {
                self.observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                owners()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&self.id);
                Ok(())
            } else {
                crate::errors::cleanup_result("bluez-acquired-sender", failures)
            }
        })
    }
}

pub(super) fn parent_key(connection: &zbus::Connection) -> String {
    format!(
        "{}:{}",
        connection.server_guid(),
        connection
            .unique_name()
            .map(|name| name.as_str())
            .unwrap_or("no-sender")
    )
}
pub(super) async fn retry_parent(parent: &str) -> Vec<DesktopError> {
    let rows: Vec<_> = owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|row| row.parent == parent)
        .cloned()
        .collect();
    let mut failures = Vec::new();
    for row in rows {
        if let Err(error) = row.close().await {
            failures.push(error);
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_opening_retains_exact_parent_and_retries_without_history() {
        let first = OpeningOwner::reserve("test-parent-first".into()).unwrap();
        let second = OpeningOwner::reserve("test-parent-second".into()).unwrap();
        let first_id = first.owner.id;
        let second_id = second.owner.id;
        drop(first);
        assert!(retry_parent("test-parent-first").await.is_empty());
        assert!(!owners().lock().unwrap().contains_key(&first_id));
        assert!(owners().lock().unwrap().contains_key(&second_id));
        second.close().await.unwrap();
        for _ in 0..1024 {
            let opening = OpeningOwner::reserve("test-parent-cycle".into()).unwrap();
            opening.close().await.unwrap();
        }
        assert!(
            !owners()
                .lock()
                .unwrap()
                .values()
                .any(|row| row.parent == "test-parent-cycle")
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; no system Bluetooth access"]
    async fn already_closed_sender_is_observed_before_cleanup_retirement() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let observer = zbus::Connection::session().await.unwrap();
        let connection = zbus::Connection::session().await.unwrap();
        let opening = OpeningOwner::reserve(parent_key(&observer)).unwrap();
        let id = opening.owner.id;
        opening.observe_with(observer.clone());
        opening.install_connection(connection.clone());
        // Controlled retained state after the previous transport shutdown:
        // zbus reports closed, while this owner still owes the sender ACK.
        connection.close().await.unwrap();
        assert!(
            opening
                .owner
                .connection
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_closed()
        );
        opening.close().await.unwrap();
        assert!(opening.owner.sender_name.lock().unwrap().is_none());
        assert!(opening.owner.observer.lock().unwrap().is_none());
        assert!(!owners().lock().unwrap().contains_key(&id));
        opening.close().await.unwrap();
        observer.close().await.unwrap();
    }
}
