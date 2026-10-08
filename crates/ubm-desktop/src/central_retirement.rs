//! Logical loss never discharges independent native discovery ownership.
use super::*;
impl<B: RadioBoundary> DesktopCentral<B> {
    pub(super) async fn release_terminal_cleanup(
        &self,
        peer: &str,
        key: &str,
        lease: &str,
        ctl: &OpControl,
        window: Window,
    ) -> Result<(), DesktopError> {
        let owned = {
            let core = self.inner.core.lock().await;
            let owner = core.holds_lease(key, lease)
                || lock_std(&self.inner.retired_leases)
                    .contains_key(&(key.to_owned(), lease.to_owned()));
            owner
                && !matches!(
                    core.connection_state(key),
                    Some(
                        ConnectionState::Connected
                            | ConnectionState::Connecting
                            | ConnectionState::Disconnecting
                    )
                )
        };
        if !owned {
            return Ok(());
        }
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.release_terminal_resources(peer),
        )
        .await
        {
            Wait::Done(result) => result,
            Wait::Expired => Err(timed_out("connection.retired-native-cleanup", window)),
            Wait::Cancelled => Err(ctl.ticket.interruption("connection.retired-native-cleanup")),
        }
    }
}
