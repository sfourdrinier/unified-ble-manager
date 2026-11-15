use super::adapter::Adapter;
use crate::{Result, api};
use async_trait::async_trait;
use bluez_async::BluetoothSession;

// Unique D-Bus names are meaningful only within a bus. Managers created for
// the same configured bus must share peer fences even if every earlier
// adapter or peripheral was dropped.
fn bus_peers(session_bus: bool) -> Result<std::sync::Arc<super::peer_source::PeerRegistry>> {
    use std::{collections::HashMap, ffi::OsString, sync::{Arc, Mutex, OnceLock}};
    type BusKey = (bool, Option<OsString>);
    static BUSES: OnceLock<Mutex<HashMap<BusKey, Arc<super::peer_source::PeerRegistry>>>> = OnceLock::new();
    let key = (session_bus, std::env::var_os(if session_bus { "DBUS_SESSION_BUS_ADDRESS" } else { "DBUS_SYSTEM_BUS_ADDRESS" }));
    let mut buses = BUSES.get_or_init(Mutex::default).lock()
        .map_err(|_| crate::Error::Other("BlueZ bus registry lock poisoned".into()))?;
    if let Some(registry) = buses.get(&key) { return Ok(registry.clone()); }
    if buses.len() >= 64 { return Err(crate::Error::Other("BlueZ bus owner capacity exhausted".into())); }
    let registry = Arc::new(super::peer_source::PeerRegistry::default());
    buses.insert(key, registry.clone());
    Ok(registry)
}

/// Implementation of [api::Manager](crate::api::Manager).
#[derive(Clone, Debug)]
pub struct Manager {
    session: BluetoothSession,
    peers: std::sync::Arc<super::peer_source::PeerRegistry>,
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let (_, session) = BluetoothSession::new().await?;
        Ok(Self {
            session,
            peers: bus_peers(false)?,
        })
    }

    /// UBM patch (UBM_PATCHES.md #3): the same as [`Manager::new`], with
    /// BlueZ reached on the D-Bus session bus instead of the system bus.
    pub async fn new_session_bus() -> Result<Self> {
        let (_, session) = BluetoothSession::new_session_bus().await?;
        Ok(Self {
            session,
            peers: bus_peers(true)?,
        })
    }

    /// Construct an isolated BlueZ manager whose peer sources retain exact
    /// native terminal evidence in the trusted host-selected store.
    pub async fn new_with_failure_evidence_store(
        failure_evidence_store: std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
        session_bus: bool,
    ) -> Result<Self> {
        let session = if session_bus {
            let (_, session) = BluetoothSession::new_session_bus().await?;
            session
        } else {
            let (_, session) = BluetoothSession::new().await?;
            session
        };
        Ok(Self {
            session,
            peers: std::sync::Arc::new(super::peer_source::PeerRegistry::new(Some(failure_evidence_store))),
        })
    }
}

#[async_trait]
impl api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        let adapters = self.session.get_adapters().await?;
        Ok(adapters
            .into_iter()
            .map(|adapter| Adapter::new(self.session.clone(), adapter.id, self.peers.clone()))
            .collect())
    }
}
