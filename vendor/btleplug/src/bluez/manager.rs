use super::adapter::Adapter;
use crate::{Result, api};
use async_trait::async_trait;
use bluez_async::BluetoothSession;

/// Implementation of [api::Manager](crate::api::Manager).
#[derive(Clone, Debug)]
pub struct Manager {
    session: BluetoothSession,
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let (_, session) = BluetoothSession::new().await?;
        Ok(Self { session })
    }

    /// UBM patch (UBM_PATCHES.md #3): the same as [`Manager::new`], with
    /// BlueZ reached on the D-Bus session bus instead of the system bus.
    pub async fn new_session_bus() -> Result<Self> {
        let (_, session) = BluetoothSession::new_session_bus().await?;
        Ok(Self { session })
    }
}

#[async_trait]
impl api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        let adapters = self.session.get_adapters().await?;
        Ok(adapters
            .into_iter()
            .map(|adapter| Adapter::new(self.session.clone(), adapter.id))
            .collect())
    }
}
