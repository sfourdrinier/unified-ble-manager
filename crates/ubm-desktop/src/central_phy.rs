//! Owned observed PHY read. Selection remains a distinct unsupported control.
use super::*;
impl<B: RadioBoundary> DesktopCentral<B> {
    pub async fn read_phy(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<crate::boundary::ObservedConnectionPhy, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.phy")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        self.require_connected_lease(&peer_key, lease, "connection.phy")
            .await?;
        let result = match drive_link(
            &self.inner,
            peer_id,
            "connection.phy",
            &ctl.ticket,
            window,
            self.inner.boundary.connection_phy(peer_id),
        )
        .await
        {
            Wait::Done(result) => result.map_err(|error| classify(error, OpKind::Read, true)),
            Wait::Expired => Err(classify(
                timed_out("connection.phy", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("connection.phy"),
                OpKind::Read,
                true,
            )),
        };
        self.name_link_end(&peer_key, result).await
    }
}
