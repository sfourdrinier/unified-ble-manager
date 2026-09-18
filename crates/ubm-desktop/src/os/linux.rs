//! Linux BlueZ adapter over D-Bus (zbus, pure Rust): what btleplug 0.12
//! does not expose (PR210 decision 7, PARITY-INVENTORY §3).
//!
//! - Link security: `Device1.Paired`/`Bonded`, `Device1.Pair` with a
//!   just-works `org.bluez.Agent1` (`NoInputNoOutput`), `CancelPairing`,
//!   `Adapter1.RemoveDevice`, and bond-change signals.
//! - Address targeting: `Adapter1.ConnectDevice` (experimental in BlueZ),
//!   else an LE discovery session on this connection until the device
//!   object exists — the legacy backend's fallback.
//! - Adapter power (`Adapter1.Powered`) read as a fact: btleplug 0.12's
//!   BlueZ `adapter_state` reports `PoweredOff` when the read itself fails.
//! - Characteristic `Flags` and the negotiated `MTU`.
//!
//! Privilege: none. The agent is exported and registered only when the host
//! asks for a pairing (never at open), under this process's own D-Bus
//! connection, which is the connection BlueZ asks for confirmations.
//! Registering an agent needs no elevated rights; a D-Bus policy that
//! refuses it surfaces as `platform.security`.
//!
//! Evidence level: type-checked from macOS (`cargo check --target
//! x86_64-unknown-linux-gnu`); the translation rules are unit-tested in
//! `os::bluez_model`. Behaviour against a live bluetoothd is unproven here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::bluez_model::{
    self, BluezCharacteristic, PairFailure, access_for_instances, bond_state,
    cancel_error_proves_terminal, classify_pair_error, device_path, device_path_for_address,
    is_unknown_method, link_mtu,
};
use crate::boundary::{
    AdapterPowerState, AddressType, BondState, CharacteristicAccess, InstanceKey, PairOutcome,
    RadioEvent, SecurityState, UnpairOutcome,
};
use crate::errors::DesktopError;

const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const ADAPTER: &str = "org.bluez.Adapter1";
const SERVICE: &str = "org.bluez.GattService1";
const CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const AGENT_PATH: &str = "/org/bluez/unifiedble/agent";
const AGENT_CAPABILITY: &str = "NoInputNoOutput";
/// Poll period while an address-targeted device object materializes
/// through discovery.
const MATERIALIZE_POLL: Duration = Duration::from_millis(100);

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// The just-works pairing agent (legacy `UbmJustWorksAgent`): confirms
/// just-works and authorization requests, refuses anything that needs
/// input it cannot supply.
struct JustWorksAgent;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl JustWorksAgent {
    fn release(&self) {}

    fn request_confirmation(&self, _device: ObjectPath<'_>, _passkey: u32) {}

    fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: String) {}

    fn request_authorization(&self, _device: ObjectPath<'_>) {}

    fn cancel(&self) {}

    fn request_pin_code(&self, _device: ObjectPath<'_>) -> Result<String, AgentError> {
        Err(AgentError::Rejected(
            "NoInputNoOutput cannot supply a PIN".to_owned(),
        ))
    }

    fn request_passkey(&self, _device: ObjectPath<'_>) -> Result<u32, AgentError> {
        Err(AgentError::Rejected(
            "NoInputNoOutput cannot supply a passkey".to_owned(),
        ))
    }

    fn display_pin_code(&self, _device: ObjectPath<'_>, _pincode: String) {}

    fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}
}

fn dbus_error_name(error: &zbus::Error) -> Option<(String, String)> {
    match error {
        zbus::Error::MethodError(name, message, _) => Some((
            name.as_str().to_owned(),
            message.clone().unwrap_or_default(),
        )),
        _ => None,
    }
}

/// Finding 113: a D-Bus failure as the platform's answer, the legacy BlueZ
/// identity `{domain:"bluez-dbus", code:<error name>}`
/// (`org.bluez.Error.Failed` when D-Bus gave none).
fn bluez_dbus_detail(error: &zbus::Error) -> crate::errors::PlatformDetail {
    let (name, message) = dbus_error_name(error)
        .unwrap_or_else(|| ("org.bluez.Error.Failed".to_owned(), error.to_string()));
    crate::errors::PlatformDetail::new("bluez-dbus", name).with_message(message)
}

fn platform(operation: &str, error: zbus::Error) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(error.to_string())
    .with_platform(bluez_dbus_detail(&error))
}

fn security(operation: &str, error: zbus::Error) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformSecurity,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(error.to_string())
    .with_platform(bluez_dbus_detail(&error))
}

fn object_path(path: &str, operation: &str) -> Result<ObjectPath<'static>, DesktopError> {
    ObjectPath::try_from(path.to_owned()).map_err(|error| {
        DesktopError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            operation,
        )
        .with_detail(format!("{path}: {error}"))
    })
}

fn bool_of(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
}

fn string_of(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok().map(str::to_owned))
}

/// Watcher and discovery-session failures (a change whose state could not
/// be read back, a watch that did not start, a discovery stop that
/// failed). Counted, never silent.
static WATCH_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Watcher and discovery-session failures since process start.
#[must_use]
pub(crate) fn security_watch_failures() -> u64 {
    WATCH_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// One D-Bus connection to bluetoothd for the parity calls of one radio.
pub(crate) struct Bluez {
    conn: zbus::Connection,
    adapter_path: String,
    agent_registered: Mutex<bool>,
    /// Peers whose `Device1.Pair` call is in flight on this connection.
    /// `CancelPairing` is only ever sent for these: BlueZ answers a
    /// `CancelPairing` with no bonding in progress by removing the bond
    /// (`src/device.c` `cancel_pairing`), which would destroy a bond a
    /// just-finished ceremony created.
    pairing: StdMutex<HashSet<String>>,
    /// Negotiated MTU per peer, read once per connection.
    mtus: StdMutex<HashMap<String, u16>>,
}

impl Bluez {
    /// Connect to BlueZ on `bus` for the adapter `adapter_id` (`hci0`) —
    /// the same bus the btleplug manager uses.
    pub(crate) async fn open(
        adapter_id: &str,
        bus: crate::boundary::BluezBus,
    ) -> Result<Arc<Self>, DesktopError> {
        let conn = match bus {
            crate::boundary::BluezBus::System => zbus::Connection::system().await,
            crate::boundary::BluezBus::Session => zbus::Connection::session().await,
        }
        .map_err(|error| {
            DesktopError::adapter_unavailable("adapter.dbus").with_detail(error.to_string())
        })?;
        Ok(Arc::new(Self {
            conn,
            adapter_path: bluez_model::adapter_path(adapter_id),
            agent_registered: Mutex::new(false),
            pairing: StdMutex::new(HashSet::new()),
            mtus: StdMutex::new(HashMap::new()),
        }))
    }

    async fn get_all(
        &self,
        path: &str,
        interface: &str,
        operation: &str,
    ) -> Result<HashMap<String, OwnedValue>, DesktopError> {
        let reply = self
            .conn
            .call_method(
                Some(BLUEZ),
                object_path(path, operation)?,
                Some(PROPERTIES),
                "GetAll",
                &(interface,),
            )
            .await
            .map_err(|error| platform(operation, error))?;
        reply
            .body()
            .deserialize::<HashMap<String, OwnedValue>>()
            .map_err(|error| platform(operation, error))
    }

    /// `Adapter1.Powered` of this radio's adapter.
    pub(crate) async fn adapter_power(&self) -> Result<AdapterPowerState, DesktopError> {
        let properties = self
            .get_all(&self.adapter_path, ADAPTER, "adapter.state")
            .await?;
        Ok(match bool_of(&properties, "Powered") {
            Some(true) => AdapterPowerState::PoweredOn,
            Some(false) => AdapterPowerState::PoweredOff,
            None => AdapterPowerState::Unknown,
        })
    }

    /// `Device1.Paired`/`Bonded` of one peer.
    pub(crate) async fn security_state(
        &self,
        peer_id: &str,
    ) -> Result<SecurityState, DesktopError> {
        let properties = self
            .get_all(&device_path(peer_id), DEVICE, "security.state")
            .await?;
        Ok(SecurityState {
            bond: bond_state(
                bool_of(&properties, "Paired"),
                bool_of(&properties, "Bonded"),
            ),
            // BlueZ has no "can pair" fact; the legacy backend's constant
            // `true` is not repeated here.
            pairing_possible: None,
        })
    }

    async fn ensure_agent(&self) -> Result<(), DesktopError> {
        let mut registered = self.agent_registered.lock().await;
        if *registered {
            return Ok(());
        }
        self.conn
            .object_server()
            .at(AGENT_PATH, JustWorksAgent)
            .await
            .map_err(|error| security("security.pair.agent", error))?;
        let outcome = self
            .conn
            .call_method(
                Some(BLUEZ),
                "/org/bluez",
                Some("org.bluez.AgentManager1"),
                "RegisterAgent",
                &(
                    object_path(AGENT_PATH, "security.pair.agent")?,
                    AGENT_CAPABILITY,
                ),
            )
            .await;
        match outcome {
            Ok(_) => {}
            Err(error)
                if dbus_error_name(&error)
                    .is_some_and(|(name, _)| name == "org.bluez.Error.AlreadyExists") => {}
            Err(error) => return Err(security("security.pair.agent", error)),
        }
        *registered = true;
        Ok(())
    }

    fn pairing(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.pairing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `Device1.Pair` through the just-works agent.
    pub(crate) async fn pair(&self, peer_id: &str) -> Result<PairOutcome, DesktopError> {
        let current = self.security_state(peer_id).await?;
        if current.bond == BondState::Bonded {
            return Ok(PairOutcome::AlreadyPaired(current));
        }
        self.ensure_agent().await?;
        let path = object_path(&device_path(peer_id), "security.pair")?;
        self.pairing().insert(peer_id.to_owned());
        let outcome = self
            .conn
            .call_method(Some(BLUEZ), path, Some(DEVICE), "Pair", &())
            .await;
        self.pairing().remove(peer_id);
        match outcome {
            Ok(_) => Ok(PairOutcome::Paired(self.security_state(peer_id).await?)),
            Err(error) => {
                let Some((name, message)) = dbus_error_name(&error) else {
                    return Err(security("security.pair", error));
                };
                match classify_pair_error(&name, &message) {
                    PairFailure::AlreadyPaired => Ok(PairOutcome::AlreadyPaired(
                        self.security_state(peer_id).await?,
                    )),
                    PairFailure::Cancelled => Ok(PairOutcome::Cancelled),
                    PairFailure::Rejected(reason) => Ok(PairOutcome::Rejected(Some(reason))),
                    PairFailure::InProgress => Err(DesktopError::new(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Platform,
                        "security.pair.arbitration",
                    )
                    .with_detail(format!("{name}: {message}"))),
                    PairFailure::Failure => Err(security("security.pair", error)),
                }
            }
        }
    }

    /// `Device1.CancelPairing`, only while this connection's own `Pair` is
    /// in flight and the bond does not exist yet (see [`Bluez::pairing`]).
    pub(crate) async fn cancel_pairing(&self, peer_id: &str) -> Result<(), DesktopError> {
        if !self.pairing().contains(peer_id) {
            return Ok(());
        }
        if self.security_state(peer_id).await?.bond == BondState::Bonded {
            return Ok(());
        }
        let path = object_path(&device_path(peer_id), "security.cancel-pairing")?;
        match self
            .conn
            .call_method(Some(BLUEZ), path, Some(DEVICE), "CancelPairing", &())
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if dbus_error_name(&error)
                    .is_some_and(|(name, _)| cancel_error_proves_terminal(&name)) =>
            {
                Ok(())
            }
            Err(error) => Err(security("security.cancel-pairing", error)),
        }
    }

    /// `Adapter1.RemoveDevice` for a bonded peer.
    pub(crate) async fn unpair(&self, peer_id: &str) -> Result<UnpairOutcome, DesktopError> {
        if self.security_state(peer_id).await?.bond != BondState::Bonded {
            return Ok(UnpairOutcome::AlreadyUnpaired);
        }
        let adapter =
            bluez_model::adapter_path_of_peer(peer_id).unwrap_or_else(|| self.adapter_path.clone());
        let device = object_path(&device_path(peer_id), "security.unpair")?;
        self.conn
            .call_method(
                Some(BLUEZ),
                object_path(&adapter, "security.unpair")?,
                Some(ADAPTER),
                "RemoveDevice",
                &(device,),
            )
            .await
            .map_err(|error| security("security.unpair", error))?;
        Ok(UnpairOutcome::Unpaired)
    }

    async fn device_exists(&self, path: &str) -> Result<bool, DesktopError> {
        match self.get_all(path, DEVICE, "peer.address-targeting").await {
            Ok(_) => Ok(true),
            Err(_) => {
                // Distinguish "no such object" from a bus failure: the
                // object manager listing is the authority.
                let managed = self.managed_objects("peer.address-targeting").await?;
                Ok(managed.keys().any(|known| known.as_str() == path))
            }
        }
    }

    /// Resolve `address` to a peer id on this adapter, materializing the
    /// device object when bluetoothd has none: `ConnectDevice` where the
    /// daemon offers it, otherwise an LE discovery session on this
    /// connection until the object appears. The caller bounds the wait.
    pub(crate) async fn resolve_address(
        &self,
        address: &str,
        address_type: AddressType,
    ) -> Result<String, DesktopError> {
        let path = device_path_for_address(&self.adapter_path, address);
        if self.device_exists(&path).await? {
            return peer_of(&path);
        }
        let mut filter: HashMap<&str, Value<'_>> = HashMap::new();
        filter.insert("Address", Value::from(address));
        filter.insert("AddressType", Value::from(address_type.as_str()));
        let adapter = object_path(&self.adapter_path, "peer.address-targeting")?;
        match self
            .conn
            .call_method(
                Some(BLUEZ),
                adapter.clone(),
                Some(ADAPTER),
                "ConnectDevice",
                &(filter,),
            )
            .await
        {
            Ok(reply) => {
                let created: OwnedObjectPath = reply
                    .body()
                    .deserialize()
                    .map_err(|error| platform("peer.address-targeting", error))?;
                return peer_of(created.as_str());
            }
            Err(error)
                if dbus_error_name(&error).is_some_and(|(name, _)| is_unknown_method(&name)) => {}
            Err(error) => return Err(platform("peer.address-targeting", error)),
        }
        // ConnectDevice is experimental in BlueZ: discover until the object
        // exists. The discovery session belongs to this connection and is
        // stopped however this call ends.
        let mut scan: HashMap<&str, Value<'_>> = HashMap::new();
        scan.insert("Transport", Value::from("le"));
        self.conn
            .call_method(
                Some(BLUEZ),
                adapter.clone(),
                Some(ADAPTER),
                "SetDiscoveryFilter",
                &(scan,),
            )
            .await
            .map_err(|error| platform("peer.address-targeting", error))?;
        self.conn
            .call_method(
                Some(BLUEZ),
                adapter.clone(),
                Some(ADAPTER),
                "StartDiscovery",
                &(),
            )
            .await
            .map_err(|error| platform("peer.address-targeting", error))?;
        let _stop = StopDiscovery {
            conn: self.conn.clone(),
            adapter: adapter.clone(),
        };
        loop {
            if self.device_exists(&path).await? {
                return peer_of(&path);
            }
            tokio::time::sleep(MATERIALIZE_POLL).await;
        }
    }

    async fn managed_objects(&self, operation: &str) -> Result<Managed, DesktopError> {
        let reply = self
            .conn
            .call_method(
                Some(BLUEZ),
                "/",
                Some(OBJECT_MANAGER),
                "GetManagedObjects",
                &(),
            )
            .await
            .map_err(|error| platform(operation, error))?;
        reply
            .body()
            .deserialize::<Managed>()
            .map_err(|error| platform(operation, error))
    }

    async fn characteristics(
        &self,
        peer_id: &str,
        operation: &str,
    ) -> Result<(HashMap<String, String>, Vec<BluezCharacteristic>), DesktopError> {
        let prefix = format!("{}/", device_path(peer_id));
        let managed = self.managed_objects(operation).await?;
        let mut services = HashMap::new();
        let mut characteristics = Vec::new();
        for (path, interfaces) in &managed {
            if !path.as_str().starts_with(&prefix) {
                continue;
            }
            if let Some(service) = interfaces.get(SERVICE)
                && let Some(uuid) = string_of(service, "UUID")
            {
                services.insert(path.as_str().to_owned(), uuid.to_ascii_lowercase());
            }
            if let Some(characteristic) = interfaces.get(CHARACTERISTIC) {
                let service_path = characteristic
                    .get("Service")
                    .and_then(|value| match &**value {
                        Value::ObjectPath(path) => Some(path.to_string()),
                        _ => None,
                    });
                let uuid = string_of(characteristic, "UUID");
                let flags = characteristic
                    .get("Flags")
                    .and_then(|value| Vec::<String>::try_from(value.try_clone().ok()?).ok())
                    .unwrap_or_default();
                let mtu = characteristic
                    .get("MTU")
                    .and_then(|value| u16::try_from(value).ok());
                if let (Some(service_path), Some(uuid)) = (service_path, uuid) {
                    characteristics.push(BluezCharacteristic {
                        path: path.as_str().to_owned(),
                        service_path,
                        uuid: uuid.to_ascii_lowercase(),
                        flags,
                        mtu,
                    });
                }
            }
        }
        Ok((services, characteristics))
    }

    /// `GattCharacteristic1.Flags` per instance (by ATT handle).
    pub(crate) async fn characteristic_access(
        &self,
        peer_id: &str,
    ) -> Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError> {
        let (services, characteristics) = self
            .characteristics(peer_id, "gatt.characteristic-access")
            .await?;
        Ok(access_for_instances(peer_id, &services, &characteristics))
    }

    /// The negotiated ATT MTU (`GattCharacteristic1.MTU`), cached per
    /// connection; `None` when the daemon does not expose it.
    pub(crate) async fn mtu(&self, peer_id: &str) -> Result<Option<u16>, DesktopError> {
        let cached = self
            .mtus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(peer_id)
            .copied();
        if cached.is_some() {
            return Ok(cached);
        }
        let (_, characteristics) = self.characteristics(peer_id, "gatt.mtu").await?;
        let mtu = link_mtu(&characteristics);
        if let Some(mtu) = mtu {
            self.mtus
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(peer_id.to_owned(), mtu);
        }
        Ok(mtu)
    }

    /// Forget per-connection facts for `peer_id`.
    pub(crate) fn forget(&self, peer_id: &str) {
        self.mtus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(peer_id);
    }

    /// Watch `Device1` changes under this adapter: bond changes become
    /// [`RadioEvent::SecurityChanged`] carrying the state read back, and a
    /// GATT database dropped under a live link (`ServicesResolved` false)
    /// becomes [`RadioEvent::ServicesChanged`] (btleplug 0.12's BlueZ
    /// backend never reports service changes).
    pub(crate) fn watch_security(
        self: &Arc<Self>,
        events: mpsc::Sender<RadioEvent>,
        spawn: &tokio::runtime::Handle,
    ) -> tokio::task::JoinHandle<()> {
        let bluez = Arc::clone(self);
        spawn.spawn(async move {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(BLUEZ)
                .and_then(|builder| builder.interface(PROPERTIES))
                .and_then(|builder| builder.member("PropertiesChanged"))
                .and_then(|builder| builder.path_namespace(bluez.adapter_path.clone()))
                .map(|builder| builder.build());
            let stream = match rule {
                Ok(rule) => zbus::MessageStream::for_match_rule(rule, &bluez.conn, None).await,
                Err(error) => Err(error),
            };
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("ubm-desktop: BlueZ bond-change watch did not start: {error}");
                    return;
                }
            };
            while let Some(message) = stream.next().await {
                let Ok(message) = message else {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                let header = message.header();
                let Some(path) = header.path().map(|path| path.as_str().to_owned()) else {
                    continue;
                };
                let Some(peer_id) = bluez_model::peer_id_for_path(&path).map(str::to_owned) else {
                    continue;
                };
                let Ok((interface, changed, _invalidated)) = message
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                if interface != DEVICE {
                    continue;
                }
                // The GATT database went away under a live link (legacy
                // `propertiesChanged`: `ServicesResolved` false). A change
                // that also drops `Connected` is the link ending, which the
                // disconnect event reports instead.
                if bool_of(&changed, "ServicesResolved") == Some(false)
                    && bool_of(&changed, "Connected") != Some(false)
                {
                    bluez.forget(&peer_id);
                    if events
                        .send(RadioEvent::ServicesChanged(peer_id.clone()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if !(changed.contains_key("Paired") || changed.contains_key("Bonded")) {
                    continue;
                }
                match bluez.security_state(&peer_id).await {
                    Ok(state) => {
                        if events
                            .send(RadioEvent::SecurityChanged { peer_id, state })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        eprintln!(
                            "ubm-desktop: BlueZ bond change on {peer_id} could not be read back: {}",
                            error.detail().unwrap_or(error.code_str())
                        );
                    }
                }
            }
        })
    }
}

impl Bluez {
    /// Watch the selected adapter's presence (finding 57): `org.bluez`
    /// losing or changing its owner (bluetoothd restart) and the adapter's
    /// `Adapter1` removed become [`RadioEvent::AdapterLost`]; the adapter's
    /// `Adapter1` added again becomes [`RadioEvent::AdapterRestored`].
    /// Power changes arrive through btleplug's own `StateUpdate`.
    pub(crate) fn watch_adapter(
        self: &Arc<Self>,
        events: mpsc::Sender<RadioEvent>,
        spawn: &tokio::runtime::Handle,
    ) -> tokio::task::JoinHandle<()> {
        let bluez = Arc::clone(self);
        spawn.spawn(async move {
            let owner_rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.DBus")
                .and_then(|builder| builder.interface("org.freedesktop.DBus"))
                .and_then(|builder| builder.member("NameOwnerChanged"))
                .and_then(|builder| builder.arg(0, BLUEZ))
                .map(|builder| builder.build());
            let objects_rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(OBJECT_MANAGER)
                .and_then(|builder| builder.path("/"))
                .map(|builder| builder.build());
            let streams = match (owner_rule, objects_rule) {
                (Ok(owner), Ok(objects)) => {
                    match (
                        zbus::MessageStream::for_match_rule(owner, &bluez.conn, None).await,
                        zbus::MessageStream::for_match_rule(objects, &bluez.conn, None).await,
                    ) {
                        (Ok(owner), Ok(objects)) => Ok((owner, objects)),
                        (Err(error), _) | (_, Err(error)) => Err(error),
                    }
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
            };
            let (owner, objects) = match streams {
                Ok(streams) => streams,
                Err(error) => {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("ubm-desktop: BlueZ adapter watch did not start: {error}");
                    return;
                }
            };
            let mut signals = futures_util::stream::select(owner, objects);
            while let Some(message) = signals.next().await {
                let Ok(message) = message else {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                let signal = match message.header().member().map(|member| member.as_str()) {
                    Some("NameOwnerChanged") => message
                        .body()
                        .deserialize::<(String, String, String)>()
                        .map(|(name, old, new)| bluez_model::name_owner_signal(&name, &old, &new)),
                    Some("InterfacesRemoved") => message
                        .body()
                        .deserialize::<(OwnedObjectPath, Vec<String>)>()
                        .map(|(path, interfaces)| {
                            bluez_model::interfaces_removed_signal(
                                &bluez.adapter_path,
                                path.as_str(),
                                &interfaces,
                            )
                        }),
                    Some("InterfacesAdded") => message
                        .body()
                        .deserialize::<(
                            OwnedObjectPath,
                            HashMap<String, HashMap<String, OwnedValue>>,
                        )>()
                        .map(|(path, interfaces)| {
                            let names: Vec<&String> = interfaces.keys().collect();
                            bluez_model::interfaces_added_signal(
                                &bluez.adapter_path,
                                path.as_str(),
                                &names,
                            )
                        }),
                    _ => Ok(None),
                };
                let event = match signal {
                    Ok(Some(bluez_model::AdapterSignal::Lost(cause))) => {
                        RadioEvent::AdapterLost(cause)
                    }
                    Ok(Some(bluez_model::AdapterSignal::Restored)) => RadioEvent::AdapterRestored,
                    Ok(None) => continue,
                    Err(error) => {
                        WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        eprintln!(
                            "ubm-desktop: BlueZ adapter signal could not be decoded: {error}"
                        );
                        continue;
                    }
                };
                if events.send(event).await.is_err() {
                    return;
                }
            }
        })
    }
}

fn peer_of(path: &str) -> Result<String, DesktopError> {
    bluez_model::peer_id_for_path(path)
        .map(str::to_owned)
        .ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                "peer.address-targeting",
            )
            .with_detail(format!("BlueZ answered a non-device object {path}"))
        })
}

/// Stops this connection's discovery session when address resolution
/// ends, however it ends (success, failure or a dropped call). The stop is
/// spawned: a failure is reported, never silently dropped.
struct StopDiscovery {
    conn: zbus::Connection,
    adapter: ObjectPath<'static>,
}

impl Drop for StopDiscovery {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let adapter = self.adapter.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("ubm-desktop: BlueZ address discovery could not be stopped: no runtime");
            return;
        };
        handle.spawn(async move {
            if let Err(error) = conn
                .call_method(Some(BLUEZ), adapter, Some(ADAPTER), "StopDiscovery", &())
                .await
            {
                WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("ubm-desktop: BlueZ address discovery stop failed: {error}");
            }
        });
    }
}
