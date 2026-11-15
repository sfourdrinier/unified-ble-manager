// native/tauri/src/ordered_recovery.rs
//! Trusted-host recovery for revoked ordered groups.

use crate::IpcValue;

impl super::BtleplugDispatcher {
    /// Lists retained ordered groups for a host-revoked caller. The trusted host
    /// can drain their exact records, retry cleanup, and acknowledge the receipt.
    pub async fn revoked_ordered_handles(
        &self,
        caller: &crate::AuthenticatedCaller,
    ) -> Vec<String> {
        let key = super::caller_key(caller);
        if !self.is_revoked(&key) {
            return Vec::new();
        }
        self.inner
            .lock()
            .await
            .callers
            .get(&key)
            .map(|owner| owner.ordered_groups.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Poll one retained record for trusted-host recovery, with the same
    /// acknowledgement semantics as the IPC stream. This does not claim the
    /// dead caller received the record.
    pub async fn recover_revoked_ordered_next(
        &self,
        caller: &crate::AuthenticatedCaller,
        handle: &str,
        ack: Option<&str>,
    ) -> IpcValue {
        let mut payload = std::collections::BTreeMap::new();
        payload.insert("orderedHandle".into(), super::string(handle));
        if let Some(ack) = ack {
            payload.insert("ackDeliveryId".into(), super::string(ack));
        }
        if !self.is_revoked(&super::caller_key(caller)) {
            return super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::OwnershipDenied,
                "cleanup",
                "tauri.ordered.recovery-owner",
            )
            .into_response();
        }
        match self.next_ordered_group(caller, payload).await {
            Ok(value) => value,
            Err(error) => error.into_response(),
        }
    }

    /// Retries native cleanup for an ordered group retained after revocation.
    pub async fn recover_revoked_ordered_close(
        &self,
        caller: &crate::AuthenticatedCaller,
        handle: &str,
    ) -> IpcValue {
        let mut payload = std::collections::BTreeMap::new();
        payload.insert("orderedHandle".into(), super::string(handle));
        if !self.is_revoked(&super::caller_key(caller)) {
            return super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::OwnershipDenied,
                "cleanup",
                "tauri.ordered.recovery-owner",
            )
            .into_response();
        }
        match self
            .close_ordered_group(caller, payload, ubm_desktop::OpControl::unbounded())
            .await
        {
            Ok(value) => value,
            Err(error) => error.into_response(),
        }
    }

    /// Retires a host-recovered group after its forget response was observed.
    pub async fn recover_revoked_ordered_retire(
        &self,
        caller: &crate::AuthenticatedCaller,
        handle: &str,
    ) -> IpcValue {
        let mut payload = std::collections::BTreeMap::new();
        payload.insert("orderedHandle".into(), super::string(handle));
        if !self.is_revoked(&super::caller_key(caller)) {
            return super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::OwnershipDenied,
                "cleanup",
                "tauri.ordered.recovery-owner",
            )
            .into_response();
        }
        match self.retire_ordered_group(caller, payload).await {
            Ok(value) => value,
            Err(error) => error.into_response(),
        }
    }

    /// Acknowledges recovery only after the host drained and released it.
    pub async fn recover_revoked_ordered_forget(
        &self,
        caller: &crate::AuthenticatedCaller,
        handle: &str,
    ) -> IpcValue {
        let mut payload = std::collections::BTreeMap::new();
        payload.insert("orderedHandle".into(), super::string(handle));
        if !self.is_revoked(&super::caller_key(caller)) {
            return super::DispatchError::new(
                ubm_core::contracts::BleErrorCode::OwnershipDenied,
                "cleanup",
                "tauri.ordered.recovery-owner",
            )
            .into_response();
        }
        match self.forget_ordered_group(caller, payload).await {
            Ok(value) => value,
            Err(error) => error.into_response(),
        }
    }
}
