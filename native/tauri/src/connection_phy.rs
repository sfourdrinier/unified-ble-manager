//! Lease-scoped native PHY observation. Selection remains unsupported on WinRT.
use super::*;

impl BtleplugDispatcher {
    pub(super) async fn read_connection_phy(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let connection = self
            .connection(caller, &payload, "tauri.connection.phy")
            .await?;
        let authority = self.ensure_authority().await?;
        let measured = authority
            .read_phy(&connection.peer_id, &connection.lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([
            ("txPhy", string(measured.tx_phy.as_str())),
            ("rxPhy", string(measured.rx_phy.as_str())),
            (
                "observedAtMonotonicMs",
                IpcValue::Number(Number::from(
                    u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                )),
            ),
            ("connectionId", string(&connection.connection_id)),
            (
                "connectionGeneration",
                string(&connection.connection_generation),
            ),
        ]))
    }
}
