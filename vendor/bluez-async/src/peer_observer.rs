// vendor/bluez-async/src/peer_observer.rs
//! Synchronous, connection-shared peer signal observation.

use crate::match_cleanup::MatchLease;
use crate::{BluetoothEvent, DeviceEvent, DeviceId};
use dbus::channel::MatchingReceiver;
use dbus::message::{MatchRule, SignalArgs};
use dbus::nonblock::stdintf::org_freedesktop_dbus::{
    ObjectManagerInterfacesAdded, ObjectManagerInterfacesRemoved,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Closure is terminal; the upstream tail cannot be proven complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerObserverClose {
    Disconnected,
    ConnectionLost,
    Unregistered,
    Overflow,
    OwnerChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerObserverEvent {
    Event(BluetoothEvent),
    GattGraphChanged,
    Closed(PeerObserverClose),
}

pub fn event_bytes(event: &PeerObserverEvent) -> usize {
    let PeerObserverEvent::Event(event) = event else {
        return 0;
    };
    let fixed = std::mem::size_of::<BluetoothEvent>();
    match event {
        BluetoothEvent::Adapter { id, .. } => fixed.saturating_add(id.to_string().len()),
        BluetoothEvent::Characteristic {
            id,
            event: crate::CharacteristicEvent::Value { value },
        } => fixed
            .saturating_add(id.to_string().len())
            .saturating_add(value.len()),
        BluetoothEvent::Device { id, event } => {
            let payload = match event {
                DeviceEvent::ManufacturerData { manufacturer_data } => {
                    manufacturer_data.values().fold(0usize, |n, value| {
                        n.saturating_add(value.len() + std::mem::size_of::<u16>())
                    })
                }
                DeviceEvent::ServiceData { service_data } => {
                    service_data.values().fold(0usize, |n, value| {
                        n.saturating_add(value.len() + std::mem::size_of::<uuid::Uuid>())
                    })
                }
                DeviceEvent::Services { services } => services
                    .len()
                    .saturating_mul(std::mem::size_of::<uuid::Uuid>()),
                DeviceEvent::PropertiesChanged { properties } => {
                    properties.iter().fold(0usize, |n, value| {
                        n.saturating_add(value.len() + std::mem::size_of::<String>())
                    })
                }
                _ => 0,
            };
            fixed
                .saturating_add(id.to_string().len())
                .saturating_add(payload)
        }
    }
}

type Callback = Box<dyn FnMut(PeerObserverEvent) + Send>;
struct Entry {
    id: u64,
    delivery: Mutex<Delivery>,
}
struct Delivery {
    callback: Option<Callback>,
    queue: VecDeque<(PeerObserverEvent, usize)>,
    queued_bytes: usize,
    max_records: usize,
    max_bytes: usize,
    running: bool,
}
impl Entry {
    // Called while the registry lock establishes event/terminal order. It never
    // invokes or drops a callback under either lock.
    fn enqueue(&self, events: impl IntoIterator<Item = PeerObserverEvent>) -> (bool, bool) {
        let mut delivery = self.delivery.lock().expect("peer delivery lock poisoned");
        let mut overflow = false;
        for event in events {
            let bytes = event_bytes(&event);
            if matches!(
                event,
                PeerObserverEvent::Event(_) | PeerObserverEvent::GattGraphChanged
            ) && (delivery.queue.len() >= delivery.max_records
                || bytes > delivery.max_bytes.saturating_sub(delivery.queued_bytes))
            {
                delivery
                    .queue
                    .push_back((PeerObserverEvent::Closed(PeerObserverClose::Overflow), 0));
                overflow = true;
                break;
            }
            delivery.queued_bytes += bytes;
            delivery.queue.push_back((event, bytes));
        }
        if delivery.running {
            return (false, overflow);
        }
        delivery.running = true;
        (true, overflow)
    }

    fn deliver(&self) {
        let mut callback = {
            let mut delivery = self.delivery.lock().expect("peer delivery lock poisoned");
            delivery
                .callback
                .take()
                .expect("idle observer owns callback")
        };
        loop {
            let event = {
                let mut delivery = self.delivery.lock().expect("peer delivery lock poisoned");
                match delivery.queue.pop_front() {
                    Some((event, bytes)) => {
                        delivery.queued_bytes -= bytes;
                        event
                    }
                    None => {
                        delivery.callback = Some(callback);
                        delivery.running = false;
                        return;
                    }
                }
            };
            callback(event);
        }
    }
}
#[derive(Default)]
struct State {
    next_id: u64,
    closed: bool,
    owner: Option<String>,
    owner_revision: u64,
    owner_watch: Option<MatchLease>,
    graph_watch: Option<MatchLease>,
    graph_added_watch: Option<MatchLease>,
    peers: HashMap<DeviceId, Arc<Entry>>,
}
#[derive(Default)]
pub(crate) struct PeerObservers(Mutex<State>);

impl PeerObservers {
    #[cfg(test)]
    fn insert(&self, device: DeviceId, callback: Callback) -> Result<u64, PeerObserverError> {
        self.insert_with_limits(device, callback, PeerObserverLimits::default(), false)
    }

    fn insert_with_limits(
        &self,
        device: DeviceId,
        callback: Callback,
        limits: PeerObserverLimits,
        require_owner: bool,
    ) -> Result<u64, PeerObserverError> {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        if state.closed {
            return Err(PeerObserverError::ConnectionLost);
        }
        if require_owner && state.owner.is_none() {
            return Err(PeerObserverError::OwnerUnavailable);
        }
        if state.peers.contains_key(&device) {
            return Err(PeerObserverError::AlreadyRegistered);
        }
        let id = state
            .next_id
            .checked_add(1)
            .ok_or(PeerObserverError::IdentityExhausted)?;
        state.next_id = id;
        state.peers.insert(
            device,
            Arc::new(Entry {
                id,
                delivery: Mutex::new(Delivery {
                    callback: Some(callback),
                    queue: VecDeque::new(),
                    queued_bytes: 0,
                    max_records: limits.max_records,
                    max_bytes: limits.max_bytes,
                    running: false,
                }),
            }),
        );
        Ok(id)
    }

    fn dispatch(&self, device: &DeviceId, id: u64, events: Vec<BluetoothEvent>) -> bool {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        let Some(entry) = state
            .peers
            .get(device)
            .filter(|entry| entry.id == id)
            .cloned()
        else {
            return false;
        };
        let disconnected = events.iter().any(|event| matches!(event, BluetoothEvent::Device { id, event: DeviceEvent::Connected { connected: false } } if id == device));
        let mut events: Vec<_> = events.into_iter().map(PeerObserverEvent::Event).collect();
        if disconnected {
            state.peers.remove(device);
            events.push(PeerObserverEvent::Closed(PeerObserverClose::Disconnected));
        }
        let (run, overflow) = entry.enqueue(events);
        if overflow {
            state.peers.remove(device);
        }
        drop(state);
        if run {
            entry.deliver();
        }
        !disconnected && !overflow
    }

    fn remove(&self, device: &DeviceId, id: u64) {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        let entry = if state.peers.get(device).is_some_and(|entry| entry.id == id) {
            state.peers.remove(device)
        } else {
            None
        };
        let run = entry.as_ref().is_some_and(|entry| {
            entry
                .enqueue([PeerObserverEvent::Closed(PeerObserverClose::Unregistered)])
                .0
        });
        drop(state);
        if let Some(entry) = entry {
            if run {
                entry.deliver();
            }
        }
    }

    fn registered_owner(&self, device: &DeviceId, id: u64) -> Option<String> {
        let state = self.0.lock().expect("peer observer lock poisoned");
        state.peers.get(device).filter(|entry| entry.id == id)?;
        state.owner.clone()
    }
    pub(crate) fn current_owner(&self) -> Option<String> {
        self.0
            .lock()
            .expect("peer observer lock poisoned")
            .owner
            .clone()
    }
    fn apply_owner_lookup(&self, revision: u64, owner: String) -> Result<(), PeerObserverError> {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        if state.closed {
            return Err(PeerObserverError::ConnectionLost);
        }
        if state.owner_revision == revision {
            state.owner = Some(owner);
        }
        if state.owner.is_none() {
            return Err(PeerObserverError::OwnerUnavailable);
        }
        Ok(())
    }

    fn owner_changed(&self, owner: Option<String>) {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        state.owner_revision = state.owner_revision.saturating_add(1);
        if state.owner == owner {
            return;
        }
        state.owner = owner;
        let deliveries: Vec<_> = std::mem::take(&mut state.peers)
            .into_values()
            .map(|entry| {
                let run = entry
                    .enqueue([PeerObserverEvent::Closed(PeerObserverClose::OwnerChanged)])
                    .0;
                (entry, run)
            })
            .collect();
        drop(state);
        for (entry, run) in deliveries {
            if run {
                entry.deliver();
            }
        }
    }

    pub(crate) fn connection_lost(&self) {
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        state.closed = true;
        state.owner = None;
        let deliveries: Vec<_> = std::mem::take(&mut state.peers)
            .into_values()
            .map(|entry| {
                let run = entry
                    .enqueue([PeerObserverEvent::Closed(PeerObserverClose::ConnectionLost)])
                    .0;
                (entry, run)
            })
            .collect();
        drop(state);
        for (entry, run) in deliveries {
            if run {
                entry.deliver();
            }
        }
    }
}

/// Bounds queued callback records and their accounted payload bytes. A terminal
/// has a reserved slot and reports an uncertain tail on saturation.
#[derive(Clone, Copy, Debug)]
pub struct PeerObserverLimits {
    pub max_records: usize,
    pub max_bytes: usize,
}
impl Default for PeerObserverLimits {
    fn default() -> Self {
        Self {
            max_records: 256,
            max_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PeerObserverError {
    #[error("peer already has an observer on this connection")]
    AlreadyRegistered,
    #[error("D-Bus connection has closed")]
    ConnectionLost,
    #[error("BlueZ owner is unavailable")]
    OwnerUnavailable,
    #[error("peer observer identity exhausted")]
    IdentityExhausted,
    #[error("could not register D-Bus match: {0}")]
    Match(#[from] crate::BluetoothError),
}

/// Keeps one peer callback and its shared server-match lease alive.
/// Callbacks run without registry locks. Reentrant teardown queues its terminal
/// behind an already-running callback.
pub struct PeerObserverLease {
    device: DeviceId,
    id: u64,
    daemon: String,
    observers: Arc<PeerObservers>,
    _lease: MatchLease,
}
impl PeerObserverLease {
    /// Exact BlueZ unique owner bound to this observer registration.
    pub fn daemon_owner(&self) -> &str {
        &self.daemon
    }
}
impl Drop for PeerObserverLease {
    fn drop(&mut self) {
        self.observers.remove(&self.device, self.id);
    }
}

impl PeerObservers {
    fn dispatch_message(&self, sender: &str, events: Vec<BluetoothEvent>) {
        let Some(device) = events.iter().find_map(|event| match event {
            BluetoothEvent::Device { id, .. } => Some(id.clone()),
            BluetoothEvent::Characteristic { id, .. } => Some(id.service().device()),
            BluetoothEvent::Adapter { .. } => None,
        }) else {
            return;
        };
        let id = {
            let state = self.0.lock().expect("peer observer lock poisoned");
            if state.owner.as_deref() != Some(sender) {
                return;
            }
            state.peers.get(&device).map(|entry| entry.id)
        };
        let Some(id) = id else {
            return;
        };
        let mut accepted = Vec::new();
        for event in events {
            let terminal = matches!(&event, BluetoothEvent::Device { id, event: DeviceEvent::Connected { connected: false } } if id == &device);
            accepted.push(event);
            if terminal {
                break;
            }
        }
        self.dispatch(&device, id, accepted);
    }

    fn added_graph(&self, sender: &str, object: &str, interfaces: &[String]) {
        let names: Vec<String> = interfaces
            .iter()
            .filter(|name| name.as_str() != "org.bluez.Device1")
            .cloned()
            .collect();
        self.removed_graph(sender, object, &names);
    }
    fn removed_graph(&self, sender: &str, object: &str, interfaces: &[String]) {
        if !interfaces.iter().any(|name| {
            matches!(
                name.as_str(),
                "org.bluez.GattService1"
                    | "org.bluez.GattCharacteristic1"
                    | "org.bluez.GattDescriptor1"
                    | "org.bluez.Device1"
            )
        }) {
            return;
        }
        let Some((device_path, _)) = object.split_once("/service") else {
            // Device1 removal uses the device object itself.
            if !interfaces.iter().any(|name| name == "org.bluez.Device1") {
                return;
            }
            return self.dispatch_device_removed(sender, object);
        };
        self.dispatch_graph_change(sender, device_path);
    }
    fn dispatch_device_removed(&self, sender: &str, device_path: &str) {
        let device = DeviceId::new(device_path);
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        if state.owner.as_deref() != Some(sender) {
            return;
        }
        let Some(entry) = state.peers.remove(&device) else {
            return;
        };
        let run = entry
            .enqueue([PeerObserverEvent::Closed(PeerObserverClose::Disconnected)])
            .0;
        drop(state);
        if run {
            entry.deliver();
        }
    }

    fn dispatch_graph_change(&self, sender: &str, device_path: &str) {
        let device = DeviceId::new(device_path);
        let mut state = self.0.lock().expect("peer observer lock poisoned");
        if state.owner.as_deref() != Some(sender) {
            return;
        }
        let Some(entry) = state.peers.get(&device).cloned() else {
            return;
        };
        let (run, overflow) = entry.enqueue([PeerObserverEvent::GattGraphChanged]);
        if overflow {
            state.peers.remove(&device);
        }
        drop(state);
        if run {
            entry.deliver();
        }
    }

    pub(crate) fn install(
        self: &Arc<Self>,
        connection: &Arc<dbus::nonblock::SyncConnection>,
        destination: &str,
    ) {
        let owner_rule =
            MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged").static_clone();
        let owner_observers = Arc::downgrade(self);
        let owner_destination = destination.to_owned();
        connection.start_receive(
            owner_rule,
            Box::new(move |message, _| {
                let Some(owner_observers) = owner_observers.upgrade() else {
                    return false;
                };
                if message.sender().as_deref() != Some("org.freedesktop.DBus") {
                    return true;
                }
                if let Ok((name, _, new_owner)) = message.read3::<String, String, String>() {
                    if name == owner_destination {
                        owner_observers.owner_changed((!new_owner.is_empty()).then_some(new_owner));
                    }
                }
                true
            }),
        );
        let removed_rule = ObjectManagerInterfacesRemoved::match_rule(None, None).static_clone();
        let removed_observers = Arc::downgrade(self);
        connection.start_receive(
            removed_rule,
            Box::new(move |message, _| {
                let Some(observers) = removed_observers.upgrade() else {
                    return false;
                };
                let Some(sender) = message.sender() else {
                    return true;
                };
                if let Some(removed) = ObjectManagerInterfacesRemoved::from_message(&message) {
                    observers.removed_graph(&sender, &removed.object, &removed.interfaces);
                }
                true
            }),
        );
        let added_rule = ObjectManagerInterfacesAdded::match_rule(None, None).static_clone();
        let added_observers = Arc::downgrade(self);
        connection.start_receive(
            added_rule,
            Box::new(move |message, _| {
                let Some(observers) = added_observers.upgrade() else {
                    return false;
                };
                let Some(sender) = message.sender() else {
                    return true;
                };
                if let Some(added) = ObjectManagerInterfacesAdded::from_message(&message) {
                    let names: Vec<String> = added.interfaces.keys().cloned().collect();
                    observers.added_graph(&sender, &added.object, &names);
                }
                true
            }),
        );
        let mut rule = BluetoothEvent::match_rules(None::<DeviceId>, false)
            .into_iter()
            .next()
            .expect("properties rule");
        rule.sender = Some(destination.to_owned().into());
        let observers = Arc::downgrade(self);
        connection.start_receive(
            rule,
            Box::new(move |message, _| {
                let Some(observers) = observers.upgrade() else {
                    return false;
                };
                let Some(sender) = message.sender() else {
                    return true;
                };
                let sender = sender.to_string();
                if observers
                    .0
                    .lock()
                    .expect("peer observer lock poisoned")
                    .owner
                    .as_deref()
                    != Some(sender.as_str())
                {
                    return true;
                }
                observers.dispatch_message(&sender, BluetoothEvent::message_to_events(message));
                true
            }),
        );
    }
}

impl crate::BluetoothSession {
    async fn ensure_peer_owner(&self) -> Result<(), PeerObserverError> {
        let _guard = self.peer_owner_init.lock().await;
        let needs_watch = {
            let state = self
                .peer_observers
                .0
                .lock()
                .expect("peer observer lock poisoned");
            if state.closed {
                return Err(PeerObserverError::ConnectionLost);
            }
            state.owner_watch.is_none()
        };
        if needs_watch {
            let rule =
                MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged").static_clone();
            // Scope zero is reserved for connection-owned observation; adapter
            // cleanup must not report this connection-lifetime watch as its stream.
            let lease = self
                .matches
                .acquire(rule.match_str(), 0)
                .await
                .map_err(crate::match_error)?;
            self.peer_observers
                .0
                .lock()
                .expect("peer observer lock poisoned")
                .owner_watch = Some(lease);
        }
        let needs_graph_watch = self
            .peer_observers
            .0
            .lock()
            .expect("peer observer lock poisoned")
            .graph_watch
            .is_none();
        if needs_graph_watch {
            let mut rule = ObjectManagerInterfacesRemoved::match_rule(None, None).static_clone();
            rule.sender = Some(self.destination.clone().into());
            let lease = self
                .matches
                .acquire(rule.match_str(), 0)
                .await
                .map_err(crate::match_error)?;
            self.peer_observers
                .0
                .lock()
                .expect("peer observer lock poisoned")
                .graph_watch = Some(lease);
        }
        let needs_added_watch = self
            .peer_observers
            .0
            .lock()
            .expect("peer observer lock poisoned")
            .graph_added_watch
            .is_none();
        if needs_added_watch {
            let mut rule = ObjectManagerInterfacesAdded::match_rule(None, None).static_clone();
            rule.sender = Some(self.destination.clone().into());
            let lease = self
                .matches
                .acquire(rule.match_str(), 0)
                .await
                .map_err(crate::match_error)?;
            self.peer_observers
                .0
                .lock()
                .expect("peer observer lock poisoned")
                .graph_added_watch = Some(lease);
        }
        let revision = {
            let state = self
                .peer_observers
                .0
                .lock()
                .expect("peer observer lock poisoned");
            if state.closed {
                return Err(PeerObserverError::ConnectionLost);
            }
            // Before the server match existed, a locally observed owner could
            // have changed without a delivered signal. Always query on first install.
            if !needs_watch && state.owner.is_some() {
                return Ok(());
            }
            state.owner_revision
        };
        let proxy = dbus::nonblock::Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            std::time::Duration::from_secs(30),
            self.connection.clone(),
        );
        let (owner,): (String,) = proxy
            .method_call("org.freedesktop.DBus", "GetNameOwner", (&self.destination,))
            .await
            .map_err(crate::BluetoothError::from)?;
        self.peer_observers.apply_owner_lookup(revision, owner)
    }

    /// Install exactly one synchronous observer for this peer on the shared connection.
    /// Delivery starts at this D-Bus callback, not at radio acquisition. Events accepted
    /// before registration or lost upstream are outside this observer's evidence.
    pub async fn observe_peer(
        &self,
        device: &DeviceId,
        callback: impl FnMut(PeerObserverEvent) + Send + 'static,
    ) -> Result<PeerObserverLease, PeerObserverError> {
        self.observe_peer_with_limits(device, PeerObserverLimits::default(), callback)
            .await
    }

    pub async fn observe_peer_with_limits(
        &self,
        device: &DeviceId,
        limits: PeerObserverLimits,
        callback: impl FnMut(PeerObserverEvent) + Send + 'static,
    ) -> Result<PeerObserverLease, PeerObserverError> {
        self.ensure_peer_owner().await?;
        let mut rule = BluetoothEvent::match_rules(Some(device.clone()), false)
            .into_iter()
            .next()
            .expect("peer properties rule");
        rule.sender = Some(self.destination.clone().into());
        let lease = self
            .matches
            .acquire(rule.match_str(), self.cleanup_scope)
            .await
            .map_err(crate::match_error)?;
        let id = self.peer_observers.insert_with_limits(
            device.clone(),
            Box::new(callback),
            limits,
            true,
        )?;
        let daemon = self
            .peer_observers
            .registered_owner(device, id)
            .ok_or(PeerObserverError::OwnerUnavailable)?;
        Ok(PeerObserverLease {
            device: device.clone(),
            id,
            daemon,
            observers: self.peer_observers.clone(),
            _lease: lease,
        })
    }
}

#[cfg(test)]
#[path = "peer_observer_tests.rs"]
mod tests;
