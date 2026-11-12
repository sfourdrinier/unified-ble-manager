// vendor/btleplug/src/bluez/notification_owner.rs
//! Retained BlueZ notification activation with one private D-Bus sender.

use super::peer_source::PeerSource;
use super::peripheral::Peripheral;
use crate::api::Characteristic;
use crate::{Error, PlatformError, Result};
use bluez_async::{BluetoothError, BluetoothSession, CharacteristicId, NotificationSender};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

const OWNER_CAPACITY: usize = 4096;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static OWNERS: OnceLock<Mutex<HashMap<u64, Arc<Owner>>>> = OnceLock::new();
fn owners() -> &'static Mutex<HashMap<u64, Arc<Owner>>> {
    OWNERS.get_or_init(Mutex::default)
}

/// Physical target within this observed peer and graph lifetime.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NotificationActivationIdentity {
    peer: usize,
    watch: u64,
    revision: u64,
    daemon: String,
    characteristic: CharacteristicId,
}
pub struct NotificationActivationOwner(Arc<Owner>);
struct Owner {
    id: u64,
    handles: AtomicUsize,
    identity: NotificationActivationIdentity,
    peer: Arc<PeerSource>,
    session: BluetoothSession,
    characteristic: Characteristic,
    state: Mutex<State>,
    sender: tokio::sync::Mutex<Option<NotificationSender>>,
    monitor: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    close_gate: tokio::sync::Mutex<()>,
    changed: Notify,
}
#[derive(Clone)]
enum Phase {
    Ready,
    Starting,
    Finished(std::result::Result<(), PlatformError>, bool),
    Closed,
}
struct State {
    phase: Phase,
    cleanup_requested: bool,
    sender_failure: Option<PlatformError>,
    monitor_join_failed: bool,
    source_generation: Option<u64>,
    prepared: bool,
}
impl State {
    fn retry_needed(&self) -> bool {
        self.cleanup_requested && !matches!(self.phase, Phase::Closed)
    }
}

fn log_retained_cleanup(error: &Error) {
    match error {
        Error::Platform(detail) => log::warn!(
            "BlueZ notification cleanup retained ({} {})",
            detail.domain,
            detail.code
        ),
        _ => log::warn!("BlueZ notification cleanup retained after an error"),
    }
}
fn source_error(code: &str, message: &str) -> PlatformError {
    PlatformError::new("bluez-notification-owner", code, message)
}
fn platform(error: BluetoothError) -> PlatformError {
    match Error::from(error) {
        Error::Platform(detail) => detail,
        other => source_error("source-error", &other.to_string()),
    }
}
fn indeterminate(error: &BluetoothError) -> bool {
    match error {
        BluetoothError::DbusError(error) => !matches!(
            error.name(),
            Some(
                "org.bluez.Error.NotConnected"
                    | "org.bluez.Error.NotPermitted"
                    | "org.bluez.Error.NotSupported"
                    | "org.freedesktop.DBus.Error.UnknownObject"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.AccessDenied"
            )
        ),
        _ => true,
    }
}

impl Peripheral {
    /// Retry retained cleanup obligations belonging to this shared peer.
    pub async fn retry_notification_activation_cleanup(&self) -> Result<()> {
        let peer = Arc::as_ptr(&self.peer) as usize;
        let pending: Vec<_> = owners()
            .lock()
            .expect("notification owner vault lock poisoned")
            .values()
            .filter(|owner| owner.identity.peer == peer)
            .cloned()
            .collect();
        let mut first_error = None;
        for owner in pending {
            if !owner
                .state
                .lock()
                .expect("notification activation lock poisoned")
                .retry_needed()
            {
                continue;
            }
            if let Err(error) = owner.cleanup().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Capture the exact published route, daemon and observed peer generation.
    pub async fn notification_activation_owner(
        &self,
        characteristic: &Characteristic,
    ) -> Result<NotificationActivationOwner> {
        let (watch, revision, daemon, id) = self.peer.activation_snapshot(characteristic)?;
        if self.session.notification_daemon_owner().as_deref() != Some(daemon.as_str())
            || !self.peer.activation_current(watch, revision)
        {
            return Err(Error::NotConnected);
        }
        let owner_id = NEXT_OWNER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| {
                Error::Platform(source_error(
                    "capacity",
                    "BlueZ notification owner identity exhausted",
                ))
            })?;
        Ok(NotificationActivationOwner(Arc::new(Owner {
            id: owner_id,
            handles: AtomicUsize::new(1),
            identity: NotificationActivationIdentity {
                peer: Arc::as_ptr(&self.peer) as usize,
                watch,
                revision,
                daemon,
                characteristic: id,
            },
            peer: self.peer.clone(),
            session: self.session.clone(),
            characteristic: characteristic.clone(),
            state: Mutex::new(State {
                phase: Phase::Ready,
                cleanup_requested: false,
                sender_failure: None,
                monitor_join_failed: false,
                source_generation: None,
                prepared: false,
            }),
            sender: tokio::sync::Mutex::new(None),
            monitor: tokio::sync::Mutex::new(None),
            close_gate: tokio::sync::Mutex::new(()),
            changed: Notify::new(),
        })))
    }
}
impl Owner {
    fn current(&self) -> bool {
        self.peer
            .activation_current(self.identity.watch, self.identity.revision)
            && self.session.notification_daemon_owner().as_deref()
                == Some(self.identity.daemon.as_str())
    }
    async fn sender_failed(self: Arc<Self>, failure: bluez_async::NotificationSenderFailure) {
        let failure = PlatformError::new(failure.domain, failure.code, failure.message);
        let (should_cleanup, generation) = {
            let mut state = self
                .state
                .lock()
                .expect("notification activation lock poisoned");
            state.sender_failure = Some(failure.clone());
            if let Phase::Finished(_, unknown) = &state.phase {
                let unknown = *unknown;
                state.phase = Phase::Finished(Err(failure.clone()), unknown);
                state.cleanup_requested = true;
                (true, state.source_generation)
            } else {
                (false, state.source_generation)
            }
        };
        if let Some(generation) = generation {
            self.peer.activation_failed(
                self.identity.watch,
                self.identity.revision,
                generation,
                self.id,
                &self.characteristic,
                failure,
            );
        }
        self.changed.notify_waiters();
        if should_cleanup {
            let owner = self.clone();
            tokio::spawn(async move {
                if let Err(error) = owner.cleanup().await {
                    log_retained_cleanup(&error);
                }
            });
        }
    }

    async fn run_start(self: Arc<Self>) {
        let result = if !self.current() {
            (
                Err(source_error(
                    "stale-owner",
                    "BlueZ route is no longer current",
                )),
                false,
            )
        } else {
            let expected_generation = {
                let state = self
                    .state
                    .lock()
                    .expect("notification activation lock poisoned");
                if state.prepared {
                    state.source_generation
                } else {
                    None
                }
            };
            let binding = self.peer.start_activation(
                self.identity.watch,
                self.identity.revision,
                &self.characteristic,
                self.id,
                expected_generation,
            );
            match binding {
                Err(error) => (
                    Err(source_error("source-activation", &format!("{error:?}"))),
                    false,
                ),
                Ok(generation) => {
                    {
                        let mut state = self
                            .state
                            .lock()
                            .expect("notification activation lock poisoned");
                        state.source_generation = generation;
                    }
                    {
                        match self
                            .session
                            .notification_sender(&self.identity.daemon)
                            .await
                        {
                            Err(error) => (Err(platform(error)), false),
                            Ok(sender) if !self.current() => {
                                *self.sender.lock().await = Some(sender);
                                (
                                    Err(source_error(
                                        "stale-owner",
                                        "BlueZ route changed before StartNotify",
                                    )),
                                    false,
                                )
                            }
                            Ok(sender) => {
                                let monitor = sender.monitor();
                                let terminal_monitor = monitor.clone();
                                let weak_owner = Arc::downgrade(&self);
                                let monitor_task = tokio::spawn(async move {
                                    if let bluez_async::NotificationSenderTerminal::Failed(
                                        failure,
                                    ) = terminal_monitor.terminal().await
                                    {
                                        if let Some(owner) = weak_owner.upgrade() {
                                            owner.sender_failed(failure).await;
                                        }
                                    }
                                });
                                *self.monitor.lock().await = Some(monitor_task);
                                let start = tokio::select! {
                                    result = sender.start(&self.identity.characteristic) => (Some(result), false),
                                    _ = self.peer.wait_link_end(self.identity.watch) => (None, true),
                                    _ = monitor.terminal() => (None, false),
                                };
                                let result = match start {
                                    (None, link_ended) => (
                                        Err(if link_ended {
                                            source_error(
                                                "link-ended",
                                                "The original BlueZ link ended while StartNotify was pending",
                                            )
                                        } else {
                                            source_error(
                                                "sender-lost",
                                                "The BlueZ notification sender ended while StartNotify was pending",
                                            )
                                        }),
                                        true,
                                    ),
                                    (Some(start), _) => match start {
                                        Err(error) => {
                                            let unknown = indeterminate(&error);
                                            (Err(platform(error)), unknown)
                                        }
                                        Ok(()) => match sender.synchronize().await {
                                            Err(error) => (Err(platform(error)), false),
                                            Ok(()) if !self.current() => (
                                                Err(source_error(
                                                    "stale-owner",
                                                    "BlueZ route changed during activation",
                                                )),
                                                false,
                                            ),
                                            Ok(()) => (Ok(()), false),
                                        },
                                    },
                                };
                                *self.sender.lock().await = Some(sender);
                                result
                            }
                        }
                    }
                }
            }
        };
        let terminal = self.peer.activation_link_ended(self.identity.watch);
        {
            let mut state = self
                .state
                .lock()
                .expect("notification activation lock poisoned");
            let outcome = match &state.sender_failure {
                Some(failure) => Err(failure.clone()),
                None => result.0,
            };
            state.phase = Phase::Finished(outcome, result.1);
            if terminal {
                state.cleanup_requested = true;
            }
        }
        self.changed.notify_waiters();
        if terminal {
            if let Err(error) = self.cleanup().await {
                log_retained_cleanup(&error);
            }
        }
    }
}
impl Clone for NotificationActivationOwner {
    fn clone(&self) -> Self {
        self.0.handles.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}
impl Drop for NotificationActivationOwner {
    fn drop(&mut self) {
        if self.0.handles.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let mut state = self
            .0
            .state
            .lock()
            .expect("notification activation lock poisoned");
        if matches!(state.phase, Phase::Ready) {
            let generation = state.source_generation;
            state.phase = Phase::Closed;
            drop(state);
            if let Some(generation) = generation {
                if let Err(error) =
                    self.0
                        .peer
                        .retire_activation(generation, &self.0.characteristic, self.0.id)
                {
                    log::warn!(
                        "BlueZ unstarted notification source retirement retained: {error:?}"
                    );
                }
            }
            return;
        }
        if matches!(state.phase, Phase::Closed) {
            return;
        }
        state.cleanup_requested = true;
        drop(state);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let owner = self.0.clone();
            runtime.spawn(async move {
                if let Err(error) = owner.cleanup().await {
                    log_retained_cleanup(&error);
                }
            });
        }
    }
}

impl Owner {
    /// Release the original private sender. A successful result acknowledges
    /// BlueZ logical registration removal, not physical CCCD completion.
    async fn cleanup(self: &Arc<Self>) -> Result<()> {
        let owner = self.clone();
        let scope = super::peer_source::GraphOperationScope::Effect(Some(self.identity.daemon.clone()));
        self.peer.own_graph_operation(scope, async move { owner.cleanup_inner().await }).await
    }

    async fn cleanup_inner(&self) -> Result<()> {
        let _gate = self.close_gate.lock().await;
        self.state
            .lock()
            .expect("notification activation lock poisoned")
            .cleanup_requested = true;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let phase = {
                let mut state = self
                    .state
                    .lock()
                    .expect("notification activation lock poisoned");
                if matches!(state.phase, Phase::Ready) {
                    if let Some(generation) = state.source_generation {
                        self.peer
                            .retire_activation(generation, &self.characteristic, self.id)
                            .map_err(|error| {
                                Error::Platform(source_error(
                                    "source-retirement",
                                    &format!("{error:?}"),
                                ))
                            })?;
                    }
                    state.phase = Phase::Closed;
                    drop(state);
                    self.changed.notify_waiters();
                    return Ok(());
                }
                state.phase.clone()
            };
            match phase {
                Phase::Starting => notified.await,
                Phase::Closed => return Ok(()),
                Phase::Ready => unreachable!("ready was closed under the state lock"),
                Phase::Finished(_, unknown) => {
                    let mut sender = self.sender.lock().await;
                    if let Some(current) = sender.as_mut() {
                        current.close().await.map_err(Error::from)?;
                    }
                    drop(sender);
                    if let Some(monitor) = self.monitor.lock().await.take() {
                        if monitor.await.is_err() {
                            self.state
                                .lock()
                                .expect("notification activation lock poisoned")
                                .monitor_join_failed = true;
                        }
                    }
                    if self
                        .state
                        .lock()
                        .expect("notification activation lock poisoned")
                        .monitor_join_failed
                        && !self.peer.activation_link_ended(self.identity.watch)
                    {
                        return Err(Error::Platform(source_error(
                            "monitor-indeterminate",
                            "Notification sender terminal monitor failed before its fence",
                        )));
                    }
                    if unknown && !self.peer.activation_link_ended(self.identity.watch) {
                        return Err(Error::Platform(source_error(
                            "cleanup-indeterminate",
                            "StartNotify outcome is unknown while the original link remains live",
                        )));
                    }
                    let generation = self
                        .state
                        .lock()
                        .expect("notification activation lock poisoned")
                        .source_generation;
                    if let Some(generation) = generation {
                        self.peer
                            .retire_activation(generation, &self.characteristic, self.id)
                            .map_err(|error| {
                                Error::Platform(source_error(
                                    "source-retirement",
                                    &format!("{error:?}"),
                                ))
                            })?;
                    }
                    *self.sender.lock().await = None;
                    self.state
                        .lock()
                        .expect("notification activation lock poisoned")
                        .phase = Phase::Closed;
                    self.changed.notify_waiters();
                    owners()
                        .lock()
                        .expect("notification owner vault lock poisoned")
                        .remove(&self.id);
                    return Ok(());
                }
            }
        }
    }
}

impl NotificationActivationOwner {
    pub fn identity(&self) -> NotificationActivationIdentity {
        self.0.identity.clone()
    }
    /// Bind this exact physical activation to the current source generation
    /// before registering its group route. This performs no radio operation.
    pub fn prepare_source_activation(&self) -> Result<u64> {
        let mut state = self
            .0
            .state
            .lock()
            .expect("notification activation lock poisoned");
        if !matches!(state.phase, Phase::Ready) {
            return Err(Error::NotConnected);
        }
        if state.prepared {
            return Ok(self.0.id);
        }
        if !self.0.current() {
            return Err(Error::NotConnected);
        }
        let generation = self
            .0
            .peer
            .start_activation(
                self.0.identity.watch,
                self.0.identity.revision,
                &self.0.characteristic,
                self.0.id,
                None,
            )
            .map_err(|error| {
                Error::Platform(source_error("source-activation", &format!("{error:?}")))
            })?
            .ok_or_else(|| {
                Error::Platform(source_error(
                    "source-activation",
                    "No source generation is active",
                ))
            })?;
        state.source_generation = Some(generation);
        state.prepared = true;
        Ok(self.0.id)
    }
    pub async fn subscribe_selecting(&self, _notify: Option<bool>) -> Result<()> {
        let owner = self.clone();
        let scope = super::peer_source::GraphOperationScope::Effect(Some(self.0.identity.daemon.clone()));
        self.0.peer.own_graph_operation(scope, async move { owner.subscribe_selecting_inner().await }).await
    }
    async fn subscribe_selecting_inner(&self) -> Result<()> {
        {
            let mut state = self
                .0
                .state
                .lock()
                .expect("notification activation lock poisoned");
            if matches!(state.phase, Phase::Ready) {
                let mut retained = owners()
                    .lock()
                    .expect("notification owner vault lock poisoned");
                if retained.len() >= OWNER_CAPACITY {
                    return Err(Error::Platform(source_error(
                        "capacity",
                        "BlueZ notification owner capacity exhausted",
                    )));
                }
                retained.insert(self.0.id, self.0.clone());
                state.phase = Phase::Starting;
                tokio::spawn(self.0.clone().run_start());
            }
        }
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let phase = self
                .0
                .state
                .lock()
                .expect("notification activation lock poisoned")
                .phase
                .clone();
            match phase {
                Phase::Ready | Phase::Starting => notified.await,
                Phase::Finished(result, _) => return result.map_err(Error::Platform),
                Phase::Closed => return Err(Error::NotConnected),
            }
        }
    }
    /// Release the original private sender and acknowledge its daemon-side retirement.
    pub async fn unsubscribe(&self) -> Result<()> {
        self.0.cleanup().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_start_failure_is_not_treated_as_no_effect() {
        let error = |name| BluetoothError::DbusError(dbus::Error::new_custom(name, "fixture"));
        assert!(indeterminate(&error("org.bluez.Error.Failed")));
        assert!(indeterminate(&error("org.freedesktop.DBus.Error.NoReply")));
        assert!(!indeterminate(&error("org.bluez.Error.NotSupported")));
    }

    #[test]
    fn retry_excludes_unmarked_live_owner() {
        let live = State {
            phase: Phase::Finished(Ok(()), false),
            cleanup_requested: false,
            sender_failure: None,
            monitor_join_failed: false,
            source_generation: None,
            prepared: false,
        };
        let failed = State {
            phase: Phase::Finished(Ok(()), false),
            cleanup_requested: true,
            sender_failure: None,
            monitor_join_failed: false,
            source_generation: None,
            prepared: false,
        };
        assert!(!live.retry_needed());
        assert!(failed.retry_needed());
    }
}

#[cfg(test)]
mod private_bus_tests {
    use super::*;
    use crate::api::Peripheral as _;
    use crate::bluez::peripheral::le_gatt_tests::Fixture;

    #[test]
    #[ignore = "private session bus only; no radio"]
    fn terminal_cleanup_preserves_external_handle_count() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let fixture = Fixture::new();
                let peripheral = fixture.peripheral().await;
                peripheral.discover_services().await.unwrap();
                let characteristic = peripheral.characteristics().into_iter().next().unwrap();
                let owner = peripheral
                    .notification_activation_owner(&characteristic)
                    .await
                    .unwrap();
                let second = owner.clone();
                assert_eq!(owner.0.handles.load(Ordering::Acquire), 2);
                peripheral.peer.close_current();
                assert!(owner.subscribe_selecting(None).await.is_err());
                assert_eq!(owner.0.handles.load(Ordering::Acquire), 2);
                let inner = owner.0.clone();
                drop(second);
                assert_eq!(inner.handles.load(Ordering::Acquire), 1);
                drop(owner);
                assert_eq!(inner.handles.load(Ordering::Acquire), 0);
            });
    }
    #[test]
    #[ignore = "private session bus only; no radio"]
    fn cancelled_waiter_retains_opening_until_reply_and_cleanup() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let fixture = Fixture::new();
                let peripheral = fixture.peripheral().await;
                peripheral.discover_services().await.unwrap();
                let characteristic = peripheral.characteristics().into_iter().next().unwrap();
                let owner = peripheral
                    .notification_activation_owner(&characteristic)
                    .await
                    .unwrap();
                let entered = fixture.hold_notify();
                let opening = owner.clone();
                let waiter = tokio::spawn(async move { opening.subscribe_selecting(None).await });
                tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
                    .await
                    .unwrap();
                waiter.abort();
                let closing = owner.clone();
                let close = tokio::spawn(async move { closing.unsubscribe().await });
                tokio::task::yield_now().await;
                assert!(!close.is_finished());
                fixture.finish_notify();
                tokio::time::timeout(std::time::Duration::from_secs(2), close)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let (started, stops) = fixture.notify_calls();
                assert_eq!(started.len(), 1);
                assert!(!fixture.sender_present(&started[0].1));
                assert!(stops.is_empty());
            });
    }
}
