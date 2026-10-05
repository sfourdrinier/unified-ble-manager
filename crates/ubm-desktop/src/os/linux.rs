//! Linux BlueZ adapter over D-Bus (zbus, pure Rust): what btleplug 0.12
//! does not expose (PR210 decision 7, PARITY-INVENTORY §3).
//!
//! - Link security: `Device1.Paired`/`Bonded`, `Device1.Pair` with a
//!   just-works `org.bluez.Agent1` (`NoInputNoOutput`), `CancelPairing`,
//!   `Adapter1.RemoveDevice`, and bond-change signals.
//! - Address targeting: an owned LE discovery session until the Device1
//!   object exists. ConnectDevice is not used: its post-browse profile
//!   auto-connect can select a Classic bearer.
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
    self, BluezCharacteristic, PAIRING_POSSIBLE, PairFailure, access_for_instances, bond_state,
    cancel_error_proves_terminal, classify_pair_error, device_path, device_path_for_address,
    link_mtu,
};
use crate::boundary::{
    AdapterPowerState, AddressType, BondState, CharacteristicAccess, InstanceKey, PairOutcome,
    RadioEvent, SecurityState, UnpairOutcome,
};
use crate::errors::DesktopError;

#[path = "bluez_discovery.rs"]
mod discovery;

const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const LE: &str = "org.bluez.Bearer.LE1";
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
    le_owner: Option<String>,
    gatt_watch: StdMutex<Result<(), DesktopError>>,
    address_discovery: Arc<discovery::DiscoveryOwner>,
}

impl Bluez {
    /// Connect to BlueZ on `bus` for the adapter `adapter_id` (`hci0`) —
    /// the same bus the btleplug manager uses.
    #[cfg(test)]
    pub(crate) async fn open(
        adapter_id: &str,
        bus: crate::boundary::BluezBus,
    ) -> Result<Arc<Self>, DesktopError> {
        Self::open_with_le_owner(adapter_id, bus, None).await
    }

    #[cfg(test)]
    pub(crate) async fn open_with_le_owner(
        adapter_id: &str,
        bus: crate::boundary::BluezBus,
        le_owner: Option<String>,
    ) -> Result<Arc<Self>, DesktopError> {
        Self::open_bound(adapter_id, bus, le_owner, false).await
    }

    /// Resolve and bind the daemon epoch natively. A supplied owner is an
    /// additional restriction, never evidence that the daemon implements an API.
    pub(crate) async fn open_authority(
        adapter_id: &str,
        bus: crate::boundary::BluezBus,
        expected_owner: Option<String>,
    ) -> Result<Arc<Self>, DesktopError> {
        Self::open_bound(adapter_id, bus, expected_owner, true).await
    }

    pub(crate) fn bound_owner(&self) -> Option<&str> {
        self.le_owner.as_deref()
    }

    async fn open_bound(
        adapter_id: &str,
        bus: crate::boundary::BluezBus,
        expected_owner: Option<String>,
        resolve_owner: bool,
    ) -> Result<Arc<Self>, DesktopError> {
        let conn = match bus {
            crate::boundary::BluezBus::System => zbus::Connection::system().await,
            crate::boundary::BluezBus::Session => zbus::Connection::session().await,
        }
        .map_err(|error| {
            DesktopError::adapter_unavailable("adapter.dbus").with_detail(error.to_string())
        })?;
        let mut authority = Self {
            conn,
            adapter_path: bluez_model::adapter_path(adapter_id),
            agent_registered: Mutex::new(false),
            pairing: StdMutex::new(HashSet::new()),
            mtus: StdMutex::new(HashMap::new()),
            le_owner: expected_owner,
            gatt_watch: StdMutex::new(Err(DesktopError::new(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "gatt.watch",
            )
            .with_detail("LE GATT observation has not been registered"))),
            address_discovery: Arc::new(discovery::DiscoveryOwner::default()),
        };
        if resolve_owner {
            let owner = authority.current_daemon_owner().await?;
            authority.le_owner = Some(owner);
            // Recheck after binding: replacement cannot turn resolution into
            // silent admission of a different daemon generation.
            authority.current_daemon_owner().await?;
        }
        Ok(Arc::new(authority))
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

    /// Read selected-adapter bond facts from one current daemon epoch.
    pub(crate) async fn bonded_peers(
        &self,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        let operation = "peers.bonded";
        let owner = self.current_daemon_owner_for(operation).await?;
        let reply = self
            .conn
            .call_method(
                Some(owner.as_str()),
                "/",
                Some(OBJECT_MANAGER),
                "GetManagedObjects",
                &(),
            )
            .await
            .map_err(|error| platform(operation, error))?;
        let managed = reply
            .body()
            .deserialize::<Managed>()
            .map_err(|error| platform(operation, error))?;
        if self.current_daemon_owner_for(operation).await? != owner {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                operation,
            )
            .with_detail(
                "the BlueZ daemon owner changed during bonded enumeration; create a fresh manager",
            ));
        }
        let mut peers = Vec::new();
        for (path, interfaces) in managed {
            let Some(properties) = interfaces.get(DEVICE) else {
                continue;
            };
            let Some(id) = bluez_model::bonded_peer_id(
                path.as_str(),
                &self.adapter_path,
                bool_of(properties, "Paired"),
                bool_of(properties, "Bonded"),
            ) else {
                continue;
            };
            peers.push(crate::boundary::DirectoryPeer {
                peer_id: id.to_owned(),
                name: string_of(properties, "Name"),
                connection: match bool_of(properties, "Connected") {
                    Some(true) => "connected",
                    Some(false) => "disconnected",
                    None => "unknown",
                },
            });
        }
        peers.sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        Ok(peers)
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
            // B-R1: BlueZ has no "can pair" fact; the legacy backend's
            // constant `true` is the contract.
            pairing_possible: Some(PAIRING_POSSIBLE),
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

    async fn device_exists(&self, path: &str, owner: &str) -> Result<bool, DesktopError> {
        match self
            .conn
            .call_method(Some(owner), path, Some(PROPERTIES), "GetAll", &(DEVICE,))
            .await
        {
            Ok(reply) => {
                reply
                    .body()
                    .deserialize::<HashMap<String, OwnedValue>>()
                    .map_err(|error| platform("peer.address-targeting", error))?;
                Ok(true)
            }
            Err(error)
                if dbus_error_name(&error).is_some_and(|(name, _)| {
                    matches!(
                        name.as_str(),
                        "org.freedesktop.DBus.Error.UnknownObject" | "org.bluez.Error.DoesNotExist"
                    )
                }) =>
            {
                Ok(false)
            }
            Err(error) => Err(platform("peer.address-targeting", error)),
        }
    }

    async fn current_daemon_owner(&self) -> Result<String, DesktopError> {
        self.current_daemon_owner_for("peer.address-targeting")
            .await
    }

    pub(crate) async fn lease_owner_retired(&self, owner: &str) -> Result<bool, DesktopError> {
        let operation = "connection.disconnect.owner-lifetime";
        zbus::names::UniqueName::try_from(owner).map_err(|error| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Cleanup,
                operation,
            )
            .with_detail(error.to_string())
        })?;
        if self.le_owner.as_deref() != Some(owner) {
            return Err(DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Cleanup,
                operation,
            )
            .with_detail("lease retirement must check its original pinned unique owner"));
        }
        let present: bool = self
            .conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(owner,),
            )
            .await
            .map_err(|error| platform(operation, error))?
            .body()
            .deserialize()
            .map_err(|error| platform(operation, error))?;
        Ok(!present)
    }

    async fn current_daemon_owner_for(&self, operation: &str) -> Result<String, DesktopError> {
        let owner: String = self
            .conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(BLUEZ,),
            )
            .await
            .map_err(|error| platform(operation, error))?
            .body()
            .deserialize()
            .map_err(|error| platform(operation, error))?;
        if self
            .le_owner
            .as_ref()
            .is_some_and(|expected| expected != &owner)
        {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                operation,
            )
            .with_detail("the bound BlueZ daemon owner changed; create a fresh manager to resolve and verify native authority"));
        }
        Ok(owner)
    }

    /// A daemon owner string is an epoch, not a capability. Verify the private
    /// implementation contract before admitting lifecycle work; individual
    /// device answers still determine actual link/discovery outcomes.
    pub(crate) async fn verify_connection_contract(&self) -> Result<(), DesktopError> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.verify_connection_contract_inner(),
        )
        .await
        .map_err(|_| {
            DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "connection.authority",
            )
            .with_detail("Linux authority contract observation timed out after five seconds")
            .with_platform(crate::errors::PlatformDetail::new(
                "ubm-linux-authority",
                "observation-timeout",
            ))
        })?
    }

    async fn verify_connection_contract_inner(&self) -> Result<(), DesktopError> {
        let owner = self.current_daemon_owner().await?;
        let reply = self
            .conn
            .call_method(
                Some(owner.as_str()),
                object_path(&self.adapter_path, "connection.authority")?,
                Some("org.unifiedblemanager.LinuxAuthority1"),
                "GetContract",
                &(),
            )
            .await
            .map_err(|error| {
                DesktopError::new(
                    BleErrorCode::CapabilityUnsupported,
                    BleErrorDomain::Capability,
                    "connection.authority",
                )
                .with_detail(
                    "the pinned daemon does not provide the required Linux authority contract",
                )
                .with_platform(bluez_dbus_detail(&error))
            })?;
        let versions: (u32, u32, u32) = reply.body().deserialize().map_err(|error| {
            DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "connection.authority",
            )
            .with_detail(format!("malformed Linux authority contract: {error}"))
            .with_platform(bluez_dbus_detail(&error))
        })?;
        if versions != (1, 2, 1) {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "connection.authority",
            )
            .with_detail(format!(
                "unsupported Linux authority contract/lease/GATT versions: {versions:?}"
            )));
        }
        self.verify_daemon_owner(&owner).await
    }

    async fn verify_daemon_owner(&self, owner: &str) -> Result<(), DesktopError> {
        if self.current_daemon_owner().await? != owner {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peer.address-targeting",
            )
            .with_detail("the BlueZ daemon owner changed during address resolution"));
        }
        Ok(())
    }

    /// Resolve `address` to a peer id on this adapter, materializing the
    /// device object through owned LE discovery, never ConnectDevice.
    /// The caller bounds the wait; accepted start/stop replies remain owned
    /// after cancellation and final transport close retries failed cleanup.
    pub(crate) async fn resolve_address(
        &self,
        address: &str,
        _address_type: AddressType,
    ) -> Result<String, DesktopError> {
        let path = device_path_for_address(&self.adapter_path, address);
        let owner = self.current_daemon_owner().await?;
        if self.device_exists(&path, &owner).await? {
            self.verify_daemon_owner(&owner).await?;
            return peer_of(&path);
        }
        let _gate = self.address_discovery.gate.lock().await;
        self.address_discovery
            .cleanup_locked(&self.conn, &self.adapter_path)
            .await?;
        let owner = self.current_daemon_owner().await?;
        let adapter = object_path(&self.adapter_path, "peer.address-targeting")?;
        // Discovery materializes an identity without acquiring any bearer.
        let mut scan: HashMap<&str, Value<'_>> = HashMap::new();
        scan.insert("Transport", Value::from("le"));
        self.conn
            .call_method(
                Some(owner.as_str()),
                adapter.clone(),
                Some(ADAPTER),
                "SetDiscoveryFilter",
                &(scan,),
            )
            .await
            .map_err(|error| platform("peer.address-targeting", error))?;
        let mut cleanup = self
            .address_discovery
            .guard(self.conn.clone(), self.adapter_path.clone());
        self.address_discovery
            .start(&self.conn, &self.adapter_path, owner.clone())
            .await?;
        loop {
            if self.device_exists(&path, &owner).await? {
                self.address_discovery
                    .cleanup_locked(&self.conn, &self.adapter_path)
                    .await?;
                cleanup.armed = false;
                self.verify_daemon_owner(&owner).await?;
                return peer_of(&path);
            }
            tokio::time::sleep(MATERIALIZE_POLL).await;
        }
    }

    pub(crate) async fn finish_discovery(&self) -> Result<(), DesktopError> {
        self.address_discovery
            .cleanup(&self.conn, &self.adapter_path)
            .await
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
    pub(crate) fn gatt_watch_health(&self) -> Result<(), DesktopError> {
        self.gatt_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn fail_gatt_watch(&self, error: DesktopError) {
        WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        eprintln!("ubm-desktop: BlueZ GATT observation failed: {error}");
        *self
            .gatt_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(error);
    }

    pub(crate) async fn watch_security(
        self: &Arc<Self>,
        events: mpsc::Sender<RadioEvent>,
        spawn: &tokio::runtime::Handle,
    ) -> Result<tokio::task::JoinHandle<()>, DesktopError> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ)
            .map(|builder| builder.build())
            .map_err(|error| platform("gatt.watch", error))?;
        // Install before returning readiness. The registered stream queues
        // events even before the spawned consumer gets its first poll.
        let mut stream = match zbus::MessageStream::for_match_rule(rule, &self.conn, None).await {
            Ok(stream) => stream,
            Err(error) => {
                let error = platform("gatt.watch", error);
                self.fail_gatt_watch(error.clone());
                return Err(error);
            }
        };
        *self
            .gatt_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Ok(());
        let bluez = Arc::clone(self);
        Ok(spawn.spawn(async move {
            // Device1 may report Connected=false and ServicesResolved=false
            // in separate, ordered signals. Remember confirmed link loss so
            // its later database teardown cannot masquerade as a live change.
            let mut evidence = DeviceConnectionEvidence::default();
            while let Some(message) = stream.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        let error = platform("gatt.watch", error);
                        bluez.fail_gatt_watch(error.clone());
                        if events.send(RadioEvent::GattWatchFailed(error)).await.is_err() {
                            eprintln!("ubm-desktop: GATT observation failure receiver is closed");
                        }
                        return;
                    }
                };
                let header = message.header();
                // The bus authenticates the well-known sender in the match.
                // A new unique owner has a different object/connection epoch.
                let sender = header.sender().map(|sender| sender.to_string());
                if bluez.le_owner.as_ref().is_some_and(|owner| sender.as_ref() != Some(owner)) {
                    continue;
                }
                evidence.owner(sender);
                if bluez.le_owner.is_some()
                    && header.interface().map(|name| name.as_str()) == Some("org.unifiedblemanager.LELease1")
                    && header.member().map(|name| name.as_str()) == Some("PhysicalLost")
                {
                    if header.path().map(|path| path.as_str()) != Some(bluez.adapter_path.as_str()) {
                        continue;
                    }
                    match message.body().deserialize::<(OwnedObjectPath, u64, u8)>() {
                        Ok((path, physical_generation, reason)) if physical_generation != 0
                            && path.as_str().starts_with(&format!("{}/", bluez.adapter_path)) => {
                            if let Some(peer_id) = bluez_model::peer_id_for_path(path.as_str())
                                && events.send(RadioEvent::LinuxPhysicalLost {
                                    peer_id: peer_id.to_owned(), physical_generation, reason,
                                }).await.is_err() { return; }
                        }
                        Ok(_) => {
                            let error = DesktopError::new(BleErrorCode::PlatformFailure,
                                BleErrorDomain::Platform, "connection.watch.protocol")
                                .with_detail("invalid physical loss identity");
                            bluez.fail_gatt_watch(error.clone());
                            if events.send(RadioEvent::GattWatchFailed(error)).await.is_err() {
                                eprintln!("ubm-desktop: physical loss protocol failure receiver closed");
                            }
                            return;
                        }
                        Err(error) => {
                            let error = platform("connection.watch.protocol", error);
                            bluez.fail_gatt_watch(error.clone());
                            if events.send(RadioEvent::GattWatchFailed(error)).await.is_err() {
                                eprintln!("ubm-desktop: physical loss protocol failure receiver closed");
                            }
                            return;
                        }
                    }
                    continue;
                }
                if bluez.le_owner.is_some()
                    && header.interface().map(|name| name.as_str())
                        == Some("org.unifiedblemanager.LEGatt1")
                    && header.member().map(|name| name.as_str()) == Some("Invalidated")
                {
                    let Some(path) = header.path().map(|path| path.as_str()) else {
                        continue;
                    };
                    if !path.starts_with(&format!("{}/", bluez.adapter_path)) {
                        continue;
                    }
                    let Some(peer) = bluez_model::peer_id_for_path(path) else {
                        continue;
                    };
                    // This signal is also emitted at completion. Its counters
                    // are not a result or an instruction to retire a newer
                    // snapshot: only the authenticated current read can decide.
                    match message.body().deserialize::<(u64, u64)>() {
                        Ok(_) => {
                            if events.send(RadioEvent::GattInvalidationHint(peer.to_owned()))
                                .await.is_err() { return; }
                        }
                        Err(error) => {
                            let error = platform("gatt.watch.protocol", error);
                            bluez.fail_gatt_watch(error.clone());
                            if events.send(RadioEvent::GattWatchFailed(error)).await.is_err() {
                                eprintln!("ubm-desktop: GATT observation failure receiver is closed");
                            }
                            return;
                        }
                    }
                    continue;
                }
                if header.interface().map(|name| name.as_str()) == Some(OBJECT_MANAGER) {
                    if bluez.le_owner.is_some() && header.member().map(|name| name.as_str()) == Some("InterfacesRemoved") {
                        match message.body().deserialize::<(OwnedObjectPath, Vec<String>)>() {
                            Ok((path, interfaces)) => {
                            if path.as_str().starts_with(&format!("{}/", bluez.adapter_path)) {
                                if let Some(peer) = bluez_model::peer_id_for_path(path.as_str()) {
                                    if interfaces.iter().any(|name| name == DEVICE) {
                                        evidence.replace(peer, None);
                                        if events.send(RadioEvent::GattInvalidationHint(peer.to_owned())).await.is_err() { return; }
                                    }
                                } else if interfaces.iter().any(|name| name == SERVICE)
                                    && let Some((device, _)) = path.as_str().split_once("/service")
                                    && let Some(peer) = bluez_model::peer_id_for_path(device)
                                    && !evidence.disconnected.contains(peer)
                                    && events.send(RadioEvent::GattInvalidationHint(peer.to_owned())).await.is_err() {
                                    return;
                                }
                            }
                            }
                            Err(error) => {
                                WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                eprintln!("ubm-desktop: BlueZ LE lifecycle signal could not be decoded: {error}");
                            }
                        }
                        continue;
                    }
                    let object = match header.member().map(|name| name.as_str()) {
                        Some("InterfacesRemoved") => message.body()
                            .deserialize::<(OwnedObjectPath, Vec<String>)>()
                            .map(|(path, interfaces)| (path, interfaces.iter().any(|name| name == DEVICE), None)),
                        Some("InterfacesAdded") => message.body()
                            .deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>)>()
                            .map(|(path, interfaces)| {
                                let device = interfaces.get(if bluez.le_owner.is_some() { LE } else { DEVICE });
                                (path, device.is_some(), device.and_then(|properties| bool_of(properties, "Connected")))
                            }),
                        _ => continue,
                    };
                    match object {
                        Ok((path, true, connected)) if path.as_str().starts_with(&format!("{}/", bluez.adapter_path)) => {
                            if let Some(peer) = bluez_model::peer_id_for_path(path.as_str()) {
                                evidence.replace(peer, connected);
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!("ubm-desktop: BlueZ device lifecycle signal could not be decoded: {error}");
                        }
                    }
                    continue;
                }
                if header.interface().map(|name| name.as_str()) != Some(PROPERTIES)
                    || header.member().map(|name| name.as_str()) != Some("PropertiesChanged") {
                    continue;
                }
                let Some(path) = header.path().map(|path| path.as_str().to_owned()) else {
                    continue;
                };
                if !path.starts_with(&format!("{}/", bluez.adapter_path)) { continue; }
                let Some(peer_id) = bluez_model::peer_id_for_path(&path).map(str::to_owned) else {
                    continue;
                };
                let Ok((interface, changed, invalidated)) = message
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                if bluez.le_owner.is_some() && interface == LE {
                    match bool_of(&changed, "Connected") {
                        Some(connected) => {
                            evidence.replace(&peer_id, Some(connected));
                            // A peer-only false can be buffered across a newer
                            // lease. Only PhysicalLost's exact generation can
                            // invalidate the owned current connection.
                            if connected && events.send(RadioEvent::Connected(peer_id)).await.is_err() { return; }
                        }
                        None if invalidated.iter().any(|name| name == "Connected") => {
                            evidence.replace(&peer_id, None);
                        }
                        None => {}
                    }
                    continue;
                }
                if interface != DEVICE {
                    continue;
                }
                if bluez.le_owner.is_none() { match bool_of(&changed, "Connected") {
                    Some(false) => { evidence.replace(&peer_id, Some(false)); }
                    Some(true) => { evidence.replace(&peer_id, Some(true)); }
                    None if invalidated.iter().any(|name| name == "Connected") => {
                        evidence.replace(&peer_id, None);
                    }
                    None => {}
                } }
                // The GATT database went away under a live link (legacy
                // `propertiesChanged`: `ServicesResolved` false). A change
                // after or alongside confirmed `Connected=false` is the link
                // ending, which the disconnect event reports instead.
                if bluez.le_owner.is_none() && bool_of(&changed, "ServicesResolved") == Some(false)
                    && !evidence.disconnected.contains(&peer_id)
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
            let error = DesktopError::new(BleErrorCode::PlatformFailure, BleErrorDomain::Gatt, "gatt.watch")
                .with_detail("the registered LE GATT observation stream ended");
            bluez.fail_gatt_watch(error.clone());
            if events.send(RadioEvent::GattWatchFailed(error)).await.is_err() {
                eprintln!("ubm-desktop: GATT observation failure receiver is closed");
            }
        }))
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

/// One retained link-loss fact per extant Device1 object, never historical
/// removed objects or an earlier bluetoothd owner. Unknown is not false.
#[derive(Default)]
struct DeviceConnectionEvidence {
    owner: Option<String>,
    disconnected: HashSet<String>,
}

impl DeviceConnectionEvidence {
    fn owner(&mut self, owner: Option<String>) {
        if self.owner != owner {
            self.disconnected.clear();
            self.owner = owner;
        }
    }

    fn replace(&mut self, peer: &str, connected: Option<bool>) {
        if connected == Some(false) {
            self.disconnected.insert(peer.to_owned());
        } else {
            self.disconnected.remove(peer);
        }
    }
}

#[cfg(test)]
mod watch_tests {
    #[derive(Clone)]
    struct OwnerLifetimeLease {
        authority: Arc<Bluez>,
        owner: String,
        release_calls: Arc<std::sync::atomic::AtomicU64>,
    }
    impl super::super::linux_lease::LeaseClient for OwnerLifetimeLease {
        async fn owner_retired(&self) -> Result<bool, DesktopError> {
            self.authority.lease_owner_retired(&self.owner).await
        }
        fn allocate_reservation(&self) -> Result<u64, DesktopError> {
            Ok(11)
        }
        async fn reserve(&self, _: u64) -> Result<u64, DesktopError> {
            Ok(7)
        }
        async fn recover(&self, _: u64) -> Result<Option<u64>, DesktopError> {
            panic!("no indeterminate reservation")
        }
        async fn connect(&self, _: u64) -> Result<u64, DesktopError> {
            Ok(13)
        }
        async fn release(
            &self,
            _: u64,
            _: Option<u64>,
        ) -> Result<super::super::linux_lease::Receipt, DesktopError> {
            self.release_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Cleanup,
                "connection.disconnect",
            )
            .with_detail("live owner refuses release"))
        }
        async fn acknowledge(&self, _: u64) -> Result<(), DesktopError> {
            panic!("owner death must not send acknowledgment")
        }
        async fn replay_loss(&self, _: u64, _: u8) -> Result<(), DesktopError> {
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; native ownership proof only"]
    async fn private_bus_dead_unique_owner_retires_leases_without_rebinding_or_local_cleanup() {
        use super::super::linux_lease::Ledger;
        use std::sync::atomic::{AtomicU64, Ordering};
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let owner = publisher.unique_name().unwrap().to_string();
        let authority = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        assert!(!authority.lease_owner_retired(&owner).await.unwrap());
        assert!(authority.lease_owner_retired(BLUEZ).await.is_err());
        let client = OwnerLifetimeLease {
            authority: authority.clone(),
            owner: owner.clone(),
            release_calls: Arc::new(AtomicU64::new(0)),
        };
        let ledger = Ledger::default();
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        assert!(
            ledger.clone().release("peer").await.is_err(),
            "live refusing owner must retain lease"
        );
        publisher.release_name(BLUEZ).await.unwrap();
        let replacement = zbus::Connection::session().await.unwrap();
        replacement.request_name(BLUEZ).await.unwrap();
        assert!(
            !authority.lease_owner_retired(&owner).await.unwrap(),
            "well-known replacement is not unique owner death"
        );
        publisher.close().await.unwrap();
        assert!(authority.lease_owner_retired(&owner).await.unwrap());
        assert!(
            authority
                .lease_owner_retired(replacement.unique_name().unwrap().as_str())
                .await
                .is_err(),
            "wrong epoch cannot retire original owner obligations"
        );
        let observation = ledger
            .clone()
            .release_with_observation("peer")
            .await
            .unwrap();
        assert_eq!(
            observation,
            super::super::linux_lease::ReleaseObservation::default()
        );
        assert!(ledger.peers().is_empty());
        assert_eq!(
            client.release_calls.load(Ordering::Relaxed),
            1,
            "no release sent to replacement or dead owner"
        );
        assert!(ledger.retry_maintenance().await.is_empty());
        // Local cleanup is independent: its refusal is not erased by retiring daemon tokens.
        let local_failure: Result<(), DesktopError> = Err(DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Cleanup,
            "local.match.remove",
        ));
        assert!(
            ledger
                .with_release_scope("peer", Some(13), || local_failure)
                .unwrap()
                .is_err()
        );
        authority.conn.clone().close().await.unwrap();
        assert!(
            authority.lease_owner_retired(&owner).await.is_err(),
            "disconnected client bus is not evidence of daemon death"
        );
    }
    use super::*;

    struct LinuxContractFixture(Arc<StdMutex<(u32, u32, u32)>>);

    #[zbus::interface(name = "org.unifiedblemanager.LinuxAuthority1")]
    impl LinuxContractFixture {
        fn get_contract(&self) -> (u32, u32, u32) {
            *self.0.lock().unwrap()
        }
    }

    fn bonded_snapshot() -> HashMap<dbus::Path<'static>, HashMap<String, dbus::arg::PropMap>> {
        use dbus::arg::Variant;
        let mut properties = dbus::arg::PropMap::new();
        properties.insert("Paired".into(), Variant(Box::new(true)));
        properties.insert("Bonded".into(), Variant(Box::new(true)));
        properties.insert("Connected".into(), Variant(Box::new(false)));
        HashMap::from([(
            dbus::Path::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap(),
            HashMap::from([(DEVICE.to_owned(), properties)]),
        )])
    }

    async fn bonded_snapshot_fixture(
        hold: bool,
    ) -> (
        Arc<dbus::nonblock::SyncConnection>,
        Arc<StdMutex<(usize, Option<dbus::Message>)>>,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<dbus_tokio::connection::IOResourceError>,
    ) {
        use dbus::channel::{MatchingReceiver, Sender};
        let (resource, publisher) = dbus_tokio::connection::new_session_sync().unwrap();
        let worker = tokio::spawn(resource);
        publisher
            .request_name(BLUEZ, false, false, false)
            .await
            .unwrap();
        let state = Arc::new(StdMutex::new((0, None)));
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed = state.clone();
        let notified = entered.clone();
        publisher.start_receive(
            dbus::message::MatchRule::new_method_call(),
            Box::new(move |message, connection| {
                if message.member().as_deref() == Some("GetManagedObjects") {
                    let mut state = observed.lock().unwrap();
                    state.0 += 1;
                    notified.notify_one();
                    if hold {
                        state.1 = Some(message);
                    } else {
                        connection
                            .send(message.method_return().append1(bonded_snapshot()))
                            .unwrap();
                    }
                }
                true
            }),
        );
        (publisher, state, entered, worker)
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; no system Bluetooth access"]
    async fn private_bus_bonded_snapshot_owner_replacement_refuses_old_epoch() {
        use dbus::channel::Sender;
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let (publisher, state, entered, worker) = bonded_snapshot_fixture(true).await;
        let old_owner = publisher.unique_name().to_string();
        let authority = Arc::new(
            Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
                .await
                .unwrap(),
        );
        let queried = authority.clone();
        let pending = tokio::spawn(async move { queried.bonded_peers().await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        publisher.release_name(BLUEZ).await.unwrap();
        let (replacement, replacement_state, _, replacement_worker) =
            bonded_snapshot_fixture(false).await;
        assert_ne!(old_owner, replacement.unique_name().to_string());
        let held = state.lock().unwrap().1.take().expect("old snapshot held");
        publisher
            .send(held.method_return().append1(bonded_snapshot()))
            .unwrap();
        let refusal = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.code(), BleErrorCode::CapabilityUnsupported);
        assert_eq!(
            authority.bound_owner(),
            Some(old_owner.as_str()),
            "old inventory authority never rebinds"
        );
        assert!(authority.bonded_peers().await.is_err());
        assert_eq!(state.lock().unwrap().0, 1, "no second old-owner snapshot");
        assert_eq!(
            replacement_state.lock().unwrap().0,
            0,
            "old authority never queries replacement"
        );
        let fresh = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        let peers = fresh.bonded_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, "hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(peers[0].connection, "disconnected");
        assert_eq!(replacement_state.lock().unwrap().0, 1);
        worker.abort();
        replacement_worker.abort();
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; native client proof only"]
    async fn private_bus_authority_contract_requires_implemented_current_versions() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let versions = Arc::new(StdMutex::new((1, 2, 1)));
        publisher
            .object_server()
            .at("/org/bluez/hci0", LinuxContractFixture(versions.clone()))
            .await
            .unwrap();
        let authority = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        assert!(authority.verify_connection_contract().await.is_ok());
        for unsupported in [(1, 1, 1), (2, 2, 1), (1, 0, 1), (1, 2, 2)] {
            *versions.lock().unwrap() = unsupported;
            assert!(authority.verify_connection_contract().await.is_err());
        }
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_authority_resolves_owner_without_host_attestation() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let authority = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        assert_eq!(
            authority.bound_owner(),
            Some(publisher.unique_name().unwrap().as_str())
        );
        publisher.release_name(BLUEZ).await.unwrap();
        let replacement = zbus::Connection::session().await.unwrap();
        replacement.request_name(BLUEZ).await.unwrap();
        assert!(
            authority.current_daemon_owner().await.is_err(),
            "old authority must not rebind old leases"
        );
        let recovered = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        assert_eq!(
            recovered.bound_owner(),
            Some(replacement.unique_name().unwrap().as_str())
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_authority_refuses_mismatched_explicit_pin() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let unrelated = zbus::Connection::session().await.unwrap();
        assert!(
            Bluez::open_authority(
                "hci0",
                crate::boundary::BluezBus::Session,
                Some(unrelated.unique_name().unwrap().to_string())
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_owner_resolution_does_not_attest_implemented_contract() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let authority = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
            .await
            .unwrap();
        let refused = authority.verify_connection_contract().await;
        assert!(
            refused.is_err(),
            "a name owner without actual methods is not support"
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_malformed_le_gatt_signal_refuses_future_admission() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let bluez = Bluez::open_with_le_owner(
            "hci0",
            crate::boundary::BluezBus::Session,
            Some(publisher.unique_name().unwrap().to_string()),
        )
        .await
        .unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let task = bluez
            .watch_security(tx, &tokio::runtime::Handle::current())
            .await
            .unwrap();
        publisher
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                "org.unifiedblemanager.LEGatt1",
                "Invalidated",
                &(7_u32, 9_u64),
            )
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a malformed trusted invalidation cannot leave readiness healthy")
            .unwrap();
        assert!(matches!(event, RadioEvent::GattWatchFailed(_)));
        assert!(bluez.gatt_watch_health().is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_gatt_watch_ready_precedes_return_and_loss_is_retained() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let bluez = Bluez::open_with_le_owner(
            "hci0",
            crate::boundary::BluezBus::Session,
            Some(publisher.unique_name().unwrap().to_string()),
        )
        .await
        .unwrap();
        assert!(bluez.gatt_watch_health().is_err());
        let baseline = matches(&publisher).await;
        let (tx, mut rx) = mpsc::channel(16);
        let task = bluez
            .watch_security(tx, &tokio::runtime::Handle::current())
            .await
            .unwrap();
        assert!(bluez.gatt_watch_health().is_ok());
        assert!(
            matches(&publisher).await > baseline,
            "AddMatch must be installed before readiness is reported"
        );
        bluez.conn.clone().close().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, RadioEvent::GattWatchFailed(_)));
        assert!(bluez.gatt_watch_health().is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_gatt_watch_startup_refusal_never_reports_ready() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let bluez = Bluez::open_with_le_owner(
            "hci0",
            crate::boundary::BluezBus::Session,
            Some(publisher.unique_name().unwrap().to_string()),
        )
        .await
        .unwrap();
        bluez.conn.clone().close().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);
        assert!(
            bluez
                .watch_security(tx, &tokio::runtime::Handle::current())
                .await
                .is_err()
        );
        assert!(bluez.gatt_watch_health().is_err());
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_le_gatt_invalidation_is_observable_and_authenticated() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let owner = publisher.unique_name().unwrap().to_string();
        let bluez =
            Bluez::open_with_le_owner("hci0", crate::boundary::BluezBus::Session, Some(owner))
                .await
                .unwrap();
        let baseline = matches(&publisher).await;
        let (tx, mut rx) = mpsc::channel(16);
        let task = bluez
            .watch_security(tx, &tokio::runtime::Handle::current())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while matches(&publisher).await <= baseline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let outsider = zbus::Connection::session().await.unwrap();
        for connection in [&outsider, &publisher] {
            connection
                .emit_signal(
                    None::<&str>,
                    "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF",
                    "org.unifiedblemanager.LEGatt1",
                    "Invalidated",
                    &(7_u64, 9_u64),
                )
                .await
                .unwrap();
        }
        outsider
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                "org.unifiedblemanager.LEGatt1",
                "Invalidated",
                &(7_u64, 9_u64),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );
        publisher
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                "org.unifiedblemanager.LEGatt1",
                "Invalidated",
                &(7_u64, 9_u64),
            )
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a valid LE GATT re-read trigger must reach the consumer")
            .unwrap();
        assert_eq!(
            format!("{event:?}")
                .matches("hci0/dev_AA_BB_CC_DD_EE_FF")
                .count(),
            1
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session"]
    async fn private_bus_strict_le_watch_ignores_aggregate_classic_state() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let owner = publisher.unique_name().unwrap().to_string();
        let bluez =
            Bluez::open_with_le_owner("hci0", crate::boundary::BluezBus::Session, Some(owner))
                .await
                .unwrap();
        let baseline = matches(&publisher).await;
        let (tx, mut rx) = mpsc::channel(16);
        let task = bluez
            .watch_security(tx, &tokio::runtime::Handle::current())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while matches(&publisher).await <= baseline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for (interface, properties) in [
            (
                DEVICE,
                vec![("Connected", true), ("ServicesResolved", false)],
            ),
            (LE, vec![("Connected", true)]),
            (
                DEVICE,
                vec![("Connected", false), ("ServicesResolved", false)],
            ),
            (LE, vec![("Connected", false)]),
        ] {
            let properties: HashMap<_, _> = properties
                .into_iter()
                .map(|(key, value)| (key, Value::from(value)))
                .collect();
            let le_connected = interface == LE
                && properties
                    .get("Connected")
                    .and_then(|value| bool::try_from(value).ok())
                    == Some(true);
            publisher
                .emit_signal(
                    None::<&str>,
                    "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                    PROPERTIES,
                    "PropertiesChanged",
                    &(interface, properties, Vec::<String>::new()),
                )
                .await
                .unwrap();
            if le_connected {
                publisher
                    .emit_signal(
                        None::<&str>,
                        "/",
                        OBJECT_MANAGER,
                        "InterfacesRemoved",
                        &(
                            OwnedObjectPath::try_from(
                                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0001",
                            )
                            .unwrap(),
                            vec![SERVICE],
                        ),
                    )
                    .await
                    .unwrap();
            }
        }
        publisher
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci0",
                "org.unifiedblemanager.LELease1",
                "PhysicalLost",
                &(
                    OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap(),
                    9_u64,
                    3_u8,
                ),
            )
            .await
            .unwrap();
        let connected = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(connected, RadioEvent::Connected(_)),
            "aggregate Classic state must not invalidate GATT: {connected:?}"
        );
        let changed = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(changed, RadioEvent::GattInvalidationHint(_)),
            "actual live GATT removal remains a re-read trigger: {changed:?}"
        );
        let disconnected = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                disconnected,
                RadioEvent::LinuxPhysicalLost {
                    physical_generation: 9,
                    reason: 3,
                    ..
                }
            ),
            "only generation-bearing LE loss may reach teardown: {disconnected:?}"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn connection_evidence_retires_objects_and_owner_epochs() {
        let mut evidence = DeviceConnectionEvidence::default();
        evidence.owner(Some(":1.10".into()));
        for index in 0..10_000 {
            let peer = format!("hci0/dev_{index}");
            evidence.replace(&peer, Some(false));
            assert_eq!(evidence.disconnected.len(), 1);
            evidence.replace(&peer, None);
            assert!(evidence.disconnected.is_empty());
        }
        evidence.replace("hci0/dev_1", Some(false));
        evidence.owner(Some(":1.11".into()));
        assert!(evidence.disconnected.is_empty());
    }

    async fn matches(connection: &zbus::Connection) -> u32 {
        connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Debug.Stats"),
                "GetStats",
                &(),
            )
            .await
            .unwrap()
            .body()
            .deserialize::<HashMap<String, OwnedValue>>()
            .unwrap()
            .get("MatchRules")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap()
    }

    async fn changed(connection: &zbus::Connection, peer: &str, properties: &[(&str, bool)]) {
        let values: HashMap<&str, Value<'_>> = properties
            .iter()
            .map(|(key, value)| (*key, Value::from(*value)))
            .collect();
        connection
            .emit_signal(
                None::<&str>,
                format!("/org/bluez/hci0/dev_{peer}"),
                PROPERTIES,
                "PropertiesChanged",
                &(DEVICE, values, Vec::<String>::new()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires dedicated dbus-run-session, never the system bus"]
    async fn private_bus_split_disconnect_does_not_invalidate_a_live_database() {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let publisher = zbus::Connection::session().await.unwrap();
        publisher.request_name(BLUEZ).await.unwrap();
        let bluez = Bluez::open("hci0", crate::boundary::BluezBus::Session)
            .await
            .unwrap();
        let baseline = matches(&publisher).await;
        let (tx, mut rx) = mpsc::channel(16);
        let task = bluez
            .watch_security(tx, &tokio::runtime::Handle::current())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while matches(&publisher).await <= baseline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        async fn receive(rx: &mut mpsc::Receiver<RadioEvent>) -> RadioEvent {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        // A positive live-loss barrier proves this actual watcher is listening.
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_00",
            &[("Connected", true), ("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_00")
        );
        changed(&publisher, "AA_BB_CC_DD_EE_01", &[("Connected", false)]).await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_01",
            &[("ServicesResolved", false)],
        )
        .await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_01",
            &[("ServicesResolved", false)],
        )
        .await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_03",
            &[("Connected", false), ("ServicesResolved", false)],
        )
        .await;
        // Distinct peer sentinel preserves ordering without a negative sleep.
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_02",
            &[("Connected", true), ("ServicesResolved", false)],
        )
        .await;
        let observed = receive(&mut rx).await;
        assert!(
            matches!(observed, RadioEvent::ServicesChanged(ref peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_02"),
            "split link loss must not become service change: {observed:?}"
        );
        // A genuinely reconnected peer must admit a later service change.
        changed(&publisher, "AA_BB_CC_DD_EE_01", &[("Connected", true)]).await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_01",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_01")
        );
        // Absence of link evidence must not suppress an actual service event.
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_04",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_04")
        );
        let recreated = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_03").unwrap();
        publisher
            .emit_signal(
                None::<&str>,
                "/",
                OBJECT_MANAGER,
                "InterfacesRemoved",
                &(recreated.clone(), vec![DEVICE]),
            )
            .await
            .unwrap();
        let interfaces =
            HashMap::from([(DEVICE, HashMap::from([("Connected", Value::from(true))]))]);
        publisher
            .emit_signal(
                None::<&str>,
                "/",
                OBJECT_MANAGER,
                "InterfacesAdded",
                &(recreated, interfaces),
            )
            .await
            .unwrap();
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_03",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_03")
        );
        // An invalidated property is unknown, not evidence of disconnection.
        changed(&publisher, "AA_BB_CC_DD_EE_05", &[("Connected", false)]).await;
        publisher
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_05",
                PROPERTIES,
                "PropertiesChanged",
                &(
                    DEVICE,
                    HashMap::<String, Value<'_>>::new(),
                    vec!["Connected"],
                ),
            )
            .await
            .unwrap();
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_05",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_05")
        );

        // Neither a similar adapter prefix nor an unauthenticated publisher
        // may contribute a service event. A bus roundtrip orders the foreign
        // sender before the positive sentinel without negative sleeps.
        publisher
            .emit_signal(
                None::<&str>,
                "/org/bluez/hci01/dev_AA_BB_CC_DD_EE_06",
                PROPERTIES,
                "PropertiesChanged",
                &(
                    DEVICE,
                    HashMap::from([("ServicesResolved", Value::from(false))]),
                    Vec::<String>::new(),
                ),
            )
            .await
            .unwrap();
        let unauthorized = zbus::Connection::session().await.unwrap();
        changed(
            &unauthorized,
            "AA_BB_CC_DD_EE_07",
            &[("ServicesResolved", false)],
        )
        .await;
        matches(&unauthorized).await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_08",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_08")
        );

        // Confirm the tombstone was consumed before replacing the real bus
        // owner. Its successor's first signal must not inherit that evidence.
        changed(&publisher, "AA_BB_CC_DD_EE_09", &[("Connected", false)]).await;
        changed(
            &publisher,
            "AA_BB_CC_DD_EE_08",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_08")
        );
        assert!(publisher.release_name(BLUEZ).await.unwrap());
        let replacement = zbus::Connection::session().await.unwrap();
        replacement.request_name(BLUEZ).await.unwrap();
        assert_ne!(publisher.unique_name(), replacement.unique_name());
        changed(
            &replacement,
            "AA_BB_CC_DD_EE_09",
            &[("ServicesResolved", false)],
        )
        .await;
        assert!(
            matches!(receive(&mut rx).await, RadioEvent::ServicesChanged(peer) if peer == "hci0/dev_AA_BB_CC_DD_EE_09")
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
