//! One discovery owner for this D-Bus connection, independent of btleplug scan.

use super::{ADAPTER, DesktopError, WATCH_FAILURES, dbus_error_name, platform};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
struct Failure {
    error: DesktopError,
    indeterminate: bool,
    no_start: bool,
}
type Reply = Shared<BoxFuture<'static, Result<(), Failure>>>;

struct State {
    owner: String,
    start: Reply,
    stop: Option<Reply>,
}

#[derive(Clone, Copy)]
pub(super) enum DiscoveryOperation {
    AddressTargeting,
    WhenAvailable,
}

impl DiscoveryOperation {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::AddressTargeting => "peer.address-targeting",
            Self::WhenAvailable => "connection.connect.when-available",
        }
    }
}

pub(super) struct DiscoveryOwner {
    pub(super) gate: Mutex<()>,
    state: StdMutex<Option<State>>,
    scheduled: AtomicBool,
    operation: DiscoveryOperation,
}

fn call(
    connection: zbus::Connection,
    adapter: String,
    owner: String,
    method: &'static str,
    operation: DiscoveryOperation,
) -> Reply {
    async move {
        connection
            .call_method(
                Some(owner.as_str()),
                adapter.as_str(),
                Some(ADAPTER),
                method,
                &(),
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                let name = dbus_error_name(&error)
                    .map(|(name, _)| name)
                    .unwrap_or_default();
                Failure {
                    indeterminate: matches!(
                        name.as_str(),
                        "org.freedesktop.DBus.Error.NoReply"
                            | "org.freedesktop.DBus.Error.Timeout"
                            | "org.freedesktop.DBus.Error.TimedOut"
                    ),
                    no_start: matches!(
                        name.as_str(),
                        "org.freedesktop.DBus.Error.UnknownMethod"
                            | "org.freedesktop.DBus.Error.UnknownInterface"
                            | "org.bluez.Error.NotAuthorized"
                            | "org.bluez.Error.NotReady"
                    ),
                    error: platform(operation.name(), error),
                }
            })
    }
    .boxed()
    .shared()
}

impl DiscoveryOwner {
    async fn owner_retired(
        &self,
        connection: &zbus::Connection,
        owner: &str,
    ) -> Result<bool, DesktopError> {
        // Ask the bus about the original unique sender, never org.bluez's
        // replacement. An error name from a live daemon is not retirement.
        zbus::names::UniqueName::try_from(owner).map_err(|error| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::PlatformFailure,
                ubm_core::contracts::BleErrorDomain::Cleanup,
                self.operation.name(),
            )
            .with_detail(format!(
                "discovery retirement requires its captured unique owner: {error}"
            ))
        })?;
        let present: bool = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(owner,),
            )
            .await
            .map_err(|error| platform(self.operation.name(), error))?
            .body()
            .deserialize()
            .map_err(|error| platform(self.operation.name(), error))?;
        Ok(!present)
    }

    async fn settle_failed_cleanup(
        &self,
        connection: &zbus::Connection,
        owner: &str,
        failure: DesktopError,
    ) -> Result<(), DesktopError> {
        match self.owner_retired(connection, owner).await {
            Ok(true) => {
                *self.state.lock().unwrap() = None;
                Ok(())
            }
            Ok(false) => Err(failure),
            Err(observation) => {
                crate::errors::cleanup_result("bluez-dbus", vec![failure, observation])
            }
        }
    }

    pub(super) fn new(operation: DiscoveryOperation) -> Self {
        Self {
            gate: Mutex::new(()),
            state: StdMutex::new(None),
            scheduled: AtomicBool::new(false),
            operation,
        }
    }
    /// Caller holds gate for its whole resolution; accepted Start is stored
    /// before polling its reply, so cancellation cannot erase admission.
    pub(super) async fn start(
        &self,
        connection: &zbus::Connection,
        adapter: &str,
        owner: String,
    ) -> Result<(), DesktopError> {
        let start = call(
            connection.clone(),
            adapter.to_owned(),
            owner.clone(),
            "StartDiscovery",
            self.operation,
        );
        *self.state.lock().unwrap() = Some(State {
            owner,
            start: start.clone(),
            stop: None,
        });
        start.await.map_err(|failure| failure.error)
    }

    /// Caller holds gate. A refused Stop stays addressable and a pending Stop
    /// keeps its original reply instead of issuing a concurrent request. Only
    /// bus-confirmed disappearance of its captured unique owner retires debt
    /// without a Stop; no request is redirected to a replacement daemon.
    pub(super) async fn cleanup_locked(
        &self,
        connection: &zbus::Connection,
        adapter: &str,
    ) -> Result<(), DesktopError> {
        let Some((owner, start, previous_stop)) = self
            .state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| (state.owner.clone(), state.start.clone(), state.stop.clone()))
        else {
            return Ok(());
        };
        if self.owner_retired(connection, &owner).await? {
            *self.state.lock().unwrap() = None;
            return Ok(());
        }
        if let Err(failure) = start.await {
            if failure.no_start {
                *self.state.lock().unwrap() = None;
                return Ok(());
            }
            if failure.indeterminate {
                return self
                    .settle_failed_cleanup(connection, &owner, failure.error)
                    .await;
            }
        }
        let stop = match previous_stop {
            Some(stop) if !matches!(stop.peek(), Some(Err(_))) => stop,
            Some(stop)
                if stop.peek().is_some_and(|reply| {
                    reply.as_ref().is_err_and(|failure| failure.indeterminate)
                }) =>
            {
                return self
                    .settle_failed_cleanup(connection, &owner, stop.await.unwrap_err().error)
                    .await;
            }
            _ => {
                let stop = call(
                    connection.clone(),
                    adapter.to_owned(),
                    owner.clone(),
                    "StopDiscovery",
                    self.operation,
                );
                self.state
                    .lock()
                    .unwrap()
                    .as_mut()
                    .expect("gate retains discovery owner")
                    .stop = Some(stop.clone());
                stop
            }
        };
        if let Err(failure) = stop.await {
            return self
                .settle_failed_cleanup(connection, &owner, failure.error)
                .await;
        }
        *self.state.lock().unwrap() = None;
        Ok(())
    }

    pub(super) async fn cleanup(
        &self,
        connection: &zbus::Connection,
        adapter: &str,
    ) -> Result<(), DesktopError> {
        let _gate = self.gate.lock().await;
        self.cleanup_locked(connection, adapter).await
    }

    pub(super) fn guard(
        self: &Arc<Self>,
        connection: zbus::Connection,
        adapter: String,
    ) -> CleanupGuard {
        CleanupGuard {
            owner: self.clone(),
            connection,
            adapter,
            armed: true,
        }
    }
}

pub(super) struct CleanupGuard {
    owner: Arc<DiscoveryOwner>,
    connection: zbus::Connection,
    adapter: String,
    pub(super) armed: bool,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed || self.owner.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let owner = self.owner.clone();
        let connection = self.connection.clone();
        let adapter = self.adapter.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _gate = owner.gate.lock().await;
                    let result = owner.cleanup_locked(&connection, &adapter).await;
                    owner.scheduled.store(false, Ordering::SeqCst);
                    if let Err(error) = result {
                        WATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "ubm-desktop: owned address discovery cleanup retained: {}",
                            error.detail().unwrap_or(error.code_str())
                        );
                    }
                });
            }
            Err(error) => {
                self.owner.scheduled.store(false, Ordering::SeqCst);
                WATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "ubm-desktop: address discovery cleanup has no executor; retained: {error}"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "bluez_discovery_tests.rs"]
mod tests;
