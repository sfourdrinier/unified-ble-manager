//! Sender-scoped daemon leases. A logical release never fabricates ACL loss.

use crate::{BluetoothError, BluetoothSession, DeviceId};
use dbus::arg::{AppendAll, RefArg, cast};
use dbus::nonblock::Proxy;
use std::collections::VecDeque;

const INTERFACE: &str = "org.unifiedblemanager.LELease1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeLeaseReleaseScope {
    PhysicalReleased,
    ReservationReleased,
    LeaseReleasedProtected,
    LeaseReleasedIndeterminate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeLeaseReleaseReceipt {
    pub token: u64,
    pub physical_generation: u64,
    pub scope: LeLeaseReleaseScope,
    pub disconnect_reason: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid LE lease release receipt: {0}")]
pub struct LeLeaseProtocolError(String);

impl LeLeaseReleaseReceipt {
    /// Accept only the exact admitted token and physical generation. This is
    /// deliberately not convertible to `()` or a Boolean disconnected fact.
    pub fn parse(
        version: u32,
        token: u64,
        physical_generation: u64,
        scope: &str,
        disconnect_reason: Option<u8>,
        expected_token: u64,
        expected_physical_generation: Option<u64>,
    ) -> Result<Self, LeLeaseProtocolError> {
        if version != 3
            || token == 0
            || token != expected_token
            || expected_physical_generation.is_some_and(|expected| physical_generation != expected)
        {
            return Err(LeLeaseProtocolError(
                "version/token/physical_generation mismatch".into(),
            ));
        }
        let scope = match scope {
            "physical-released" if physical_generation != 0 => {
                LeLeaseReleaseScope::PhysicalReleased
            }
            "reservation-released" if physical_generation == 0 => {
                LeLeaseReleaseScope::ReservationReleased
            }
            "lease-released-protected" => LeLeaseReleaseScope::LeaseReleasedProtected,
            "lease-released-indeterminate" => LeLeaseReleaseScope::LeaseReleasedIndeterminate,
            _ => return Err(LeLeaseProtocolError(format!("unknown scope {scope:?}"))),
        };
        if disconnect_reason.is_some() && scope != LeLeaseReleaseScope::PhysicalReleased {
            return Err(LeLeaseProtocolError(
                "observed reason requires exact physical release".into(),
            ));
        }
        Ok(Self {
            token,
            physical_generation,
            scope,
            disconnect_reason,
        })
    }
}

impl BluetoothSession {
    /// Shared by every peripheral and cleanup scope on this exact D-Bus sender.
    /// A new manager cannot reset the identities of a still-live sender.
    pub fn allocate_le_reservation_id(&self) -> Result<u64, BluetoothError> {
        use std::sync::atomic::Ordering;
        self.lease_reservation_ids
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                LeLeaseProtocolError("reservation identity capacity exhausted".into()).into()
            })
    }

    async fn le_lease_call<A: AppendAll>(
        &self,
        device: &DeviceId,
        owner: &str,
        method: &str,
        arguments: A,
    ) -> Result<VecDeque<Box<dyn RefArg>>, BluetoothError> {
        if method != "ReleaseLease" && method != "RecoverLease" && method != "AckLease" {
            self.le_owner(owner).await?;
        }
        if !owner.starts_with(':') {
            return Err(LeLeaseProtocolError(
                "lease cleanup requires an exact unique owner".into(),
            )
            .into());
        }
        let destination =
            dbus::strings::BusName::new(owner.to_owned()).map_err(LeLeaseProtocolError)?;
        let proxy = Proxy::new(
            destination,
            device.adapter().object_path.clone(),
            crate::DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let result = proxy.method_call(INTERFACE, method, arguments).await?;
        // The method was sent to the exact unique owner. Do not discard an
        // accepted reservation/release receipt merely because its well-known
        // name changed while the reply was in flight: the caller must retain
        // the token belonging to that original owner for compensation. The
        // next admission rechecks the current owner before further effects.
        Ok(result)
    }

    /// The caller must retain the accepted reservation future before polling it:
    /// cancellation of a waiter does not erase this sender's daemon reservation.
    pub async fn reserve_le_lease(
        &self,
        device: &DeviceId,
        owner: &str,
        reservation_id: u64,
    ) -> Result<u64, BluetoothError> {
        if reservation_id == 0 {
            return Err(LeLeaseProtocolError("zero reservation identity".into()).into());
        }
        let raw = self
            .le_lease_call(
                device,
                owner,
                "ReserveLease",
                (device.object_path.clone(), reservation_id),
            )
            .await?;
        let token = single_token(raw)?;
        if token == 0 {
            return Err(LeLeaseProtocolError("zero reservation token".into()).into());
        }
        Ok(token)
    }

    /// Recover only an original reservation identity. Zero is authoritative
    /// no-admission with the daemon's delayed-Reserve fence already installed.
    pub async fn recover_le_lease(
        &self,
        device: &DeviceId,
        owner: &str,
        reservation_id: u64,
    ) -> Result<Option<u64>, BluetoothError> {
        if reservation_id == 0 {
            return Err(LeLeaseProtocolError("zero recovery identity".into()).into());
        }
        let raw = self
            .le_lease_call(device, owner, "RecoverLease", (reservation_id,))
            .await?;
        let token = single_token(raw)?;
        Ok((token != 0).then_some(token))
    }

    /// Retire only metadata whose terminal release receipt was consumed. An
    /// acknowledgement failure does not negate the already-observed ACL end.
    pub async fn acknowledge_le_lease(
        &self,
        device: &DeviceId,
        owner: &str,
        token: u64,
    ) -> Result<(), BluetoothError> {
        if token == 0 {
            return Err(LeLeaseProtocolError("zero acknowledgement token".into()).into());
        }
        let raw = self
            .le_lease_call(device, owner, "AckLease", (token,))
            .await?;
        if !raw.is_empty() {
            return Err(LeLeaseProtocolError("AckLease requires exact empty reply".into()).into());
        }
        Ok(())
    }

    /// Connect only the previously reserved owner token; the returned physical
    /// generation must be retained for exact-generation cleanup.
    pub async fn connect_le_lease(
        &self,
        device: &DeviceId,
        owner: &str,
        token: u64,
    ) -> Result<u64, BluetoothError> {
        if token == 0 {
            return Err(LeLeaseProtocolError("zero connection token".into()).into());
        }
        let raw = self
            .le_lease_call(device, owner, "ConnectLease", (token,))
            .await?;
        let physical_generation = single_token(raw)?;
        if physical_generation == 0 {
            return Err(LeLeaseProtocolError("zero connected physical_generation".into()).into());
        }
        Ok(physical_generation)
    }

    pub async fn release_le_lease(
        &self,
        device: &DeviceId,
        owner: &str,
        token: u64,
        expected_physical_generation: Option<u64>,
    ) -> Result<LeLeaseReleaseReceipt, BluetoothError> {
        if token == 0 {
            return Err(LeLeaseProtocolError("zero release token".into()).into());
        }
        let raw = self
            .le_lease_call(device, owner, "ReleaseLease", (token,))
            .await?;
        Ok(decode_release_receipt(
            raw,
            token,
            expected_physical_generation,
        )?)
    }
}

fn decode_release_receipt(
    raw: VecDeque<Box<dyn RefArg>>,
    token: u64,
    expected_physical_generation: Option<u64>,
) -> Result<LeLeaseReleaseReceipt, LeLeaseProtocolError> {
    if raw.len() != 6 {
        return Err(LeLeaseProtocolError(
            "ReleaseLease requires exact uttsby signature".into(),
        ));
    }
    let invalid = || LeLeaseProtocolError("ReleaseLease requires exact uttsby field types".into());
    let version = *cast::<u32>(&raw[0]).ok_or_else(invalid)?;
    let returned_token = *cast::<u64>(&raw[1]).ok_or_else(invalid)?;
    let returned_physical_generation = *cast::<u64>(&raw[2]).ok_or_else(invalid)?;
    let scope = cast::<String>(&raw[3]).ok_or_else(invalid)?;
    let has_reason = *cast::<bool>(&raw[4]).ok_or_else(invalid)?;
    let reason = *cast::<u8>(&raw[5]).ok_or_else(invalid)?;
    if !has_reason && reason != 0 {
        return Err(LeLeaseProtocolError(
            "absent reason must have canonical zero byte".into(),
        ));
    }
    LeLeaseReleaseReceipt::parse(
        version,
        returned_token,
        returned_physical_generation,
        scope,
        has_reason.then_some(reason),
        token,
        expected_physical_generation,
    )
}

fn single_token(raw: VecDeque<Box<dyn RefArg>>) -> Result<u64, LeLeaseProtocolError> {
    if raw.len() != 1 {
        return Err(LeLeaseProtocolError(
            "lease identity requires exact t signature".into(),
        ));
    }
    cast::<u64>(&raw[0])
        .copied()
        .ok_or_else(|| LeLeaseProtocolError("lease identity requires exact t field type".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_observation_requires_exact_wire_types_and_absence_encoding() {
        fn boxed(value: impl RefArg + 'static) -> Box<dyn RefArg> {
            Box::new(value)
        }
        fn packet(present: Box<dyn RefArg>, reason: Box<dyn RefArg>) -> VecDeque<Box<dyn RefArg>> {
            VecDeque::from([
                boxed(3_u32),
                boxed(7_u64),
                boxed(9_u64),
                boxed("physical-released".to_owned()),
                present,
                reason,
            ])
        }
        assert_eq!(
            decode_release_receipt(packet(boxed(true), boxed(2_u8)), 7, Some(9))
                .unwrap()
                .disconnect_reason,
            Some(2)
        );
        assert_eq!(
            decode_release_receipt(packet(boxed(false), boxed(0_u8)), 7, Some(9))
                .unwrap()
                .disconnect_reason,
            None
        );
        for raw in [
            packet(boxed(false), boxed(2_u8)),
            packet(boxed(1_u8), boxed(2_u8)),
            packet(boxed(true), boxed(2_u32)),
        ] {
            assert!(decode_release_receipt(raw, 7, Some(9)).is_err());
        }
        let mut old = packet(boxed(false), boxed(0_u8));
        old.pop_back();
        old.pop_back();
        assert!(decode_release_receipt(old, 7, Some(9)).is_err());
    }

    #[test]
    fn old_scope_only_and_non_reconciling_release_versions_are_refused() {
        assert!(
            LeLeaseReleaseReceipt::parse(1, 7, 9, "physical-released", None, 7, Some(9)).is_err()
        );
        assert!(
            LeLeaseReleaseReceipt::parse(2, 7, 9, "physical-released", None, 7, Some(9)).is_err()
        );
    }

    #[test]
    fn release_receipt_preserves_physical_vs_scoped_retirement() {
        for (wire, expected) in [
            ("physical-released", LeLeaseReleaseScope::PhysicalReleased),
            (
                "lease-released-protected",
                LeLeaseReleaseScope::LeaseReleasedProtected,
            ),
            (
                "lease-released-indeterminate",
                LeLeaseReleaseScope::LeaseReleasedIndeterminate,
            ),
        ] {
            let receipt = LeLeaseReleaseReceipt::parse(3, 7, 9, wire, None, 7, Some(9)).unwrap();
            assert_eq!(receipt.scope, expected);
            assert_eq!(receipt.token, 7);
            assert_eq!(receipt.physical_generation, 9);
        }
    }

    #[test]
    fn release_receipt_refuses_unknown_or_mismatched_identity() {
        for (version, token, physical_generation, scope) in [
            (1, 7, 9, "physical-released"),
            (3, 0, 9, "physical-released"),
            (3, 8, 9, "physical-released"),
            (3, 7, 10, "physical-released"),
            (3, 7, 9, "released"),
            (3, 7, 9, ""),
        ] {
            assert!(
                LeLeaseReleaseReceipt::parse(
                    version,
                    token,
                    physical_generation,
                    scope,
                    None,
                    7,
                    Some(9)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn token_reply_requires_exact_native_width_and_field_count() {
        fn boxed(value: impl RefArg + 'static) -> Box<dyn RefArg> {
            Box::new(value)
        }
        assert_eq!(single_token(VecDeque::from([boxed(7_u64)])).unwrap(), 7);
        assert!(single_token(VecDeque::from([boxed(7_u32)])).is_err());
        assert!(single_token(VecDeque::new()).is_err());
        assert!(single_token(VecDeque::from([boxed(7_u64), boxed(9_u64),])).is_err());
    }

    #[test]
    fn failed_acquisition_cleanup_learns_physical_generation_only_from_its_exact_token() {
        let receipt =
            LeLeaseReleaseReceipt::parse(3, 7, 9, "physical-released", Some(2), 7, None).unwrap();
        assert_eq!(receipt.physical_generation, 9);
        assert_eq!(receipt.disconnect_reason, Some(2));
        assert!(
            LeLeaseReleaseReceipt::parse(3, 8, 9, "physical-released", Some(2), 7, None).is_err()
        );
    }

    #[test]
    fn physical_release_never_claims_an_unobserved_generation_ended() {
        assert!(
            LeLeaseReleaseReceipt::parse(3, 7, 0, "physical-released", Some(2), 7, None).is_err()
        );
        let reservation =
            LeLeaseReleaseReceipt::parse(3, 7, 0, "reservation-released", None, 7, None).unwrap();
        assert_eq!(reservation.scope, LeLeaseReleaseScope::ReservationReleased);
        assert!(
            LeLeaseReleaseReceipt::parse(3, 7, 9, "reservation-released", None, 7, None).is_err()
        );
        assert!(
            LeLeaseReleaseReceipt::parse(3, 7, 0, "reservation-released", None, 7, Some(9))
                .is_err()
        );
        for scope in [
            "reservation-released",
            "lease-released-protected",
            "lease-released-indeterminate",
        ] {
            assert!(LeLeaseReleaseReceipt::parse(3, 7, 0, scope, Some(2), 7, None).is_err());
        }
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; native client proof only"]
    async fn private_bus_late_reservation_retains_original_token_after_owner_replacement() {
        use dbus::channel::{MatchingReceiver, Sender};
        use dbus::message::MatchRule;
        use std::sync::{Arc, Mutex};
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let (resource, publisher) = dbus_tokio::connection::new_session_sync().unwrap();
        let publisher_task = tokio::spawn(resource);
        publisher
            .request_name("org.bluez", false, false, false)
            .await
            .unwrap();
        let pending = Arc::new(Mutex::new(None));
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed_pending = pending.clone();
        let observed_entered = entered.clone();
        let responder = publisher.clone();
        publisher.start_receive(
            MatchRule::new_method_call(),
            Box::new(move |message, _| {
                assert_eq!(message.interface().as_deref(), Some(INTERFACE));
                if message.member().as_deref() == Some("ReleaseLease") {
                    assert_eq!(message.read1::<u64>().unwrap(), 7);
                    responder
                        .send(
                            message
                                .method_return()
                                .append2(3_u32, 7_u64)
                                .append2(0_u64, "reservation-released")
                                .append2(false, 0_u8),
                        )
                        .unwrap();
                    return true;
                }
                if message.member().as_deref() == Some("RecoverLease") {
                    assert_eq!(message.read1::<u64>().unwrap(), 11);
                    responder
                        .send(message.method_return().append1(7_u64))
                        .unwrap();
                    return true;
                }
                assert_eq!(message.member().as_deref(), Some("ReserveLease"));
                *observed_pending.lock().unwrap() = Some(message);
                observed_entered.notify_one();
                true
            }),
        );
        let (resource, connection) = dbus_tokio::connection::new_session_sync().unwrap();
        let client_task = tokio::spawn(resource);
        let client = BluetoothSession::from_connection(connection);
        let original_owner = publisher.unique_name().to_string();
        let owned_client = client.clone();
        let accepted_owner = original_owner.clone();
        let device = DeviceId::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        let accepted_device = device.clone();
        let accepted = tokio::spawn(async move {
            owned_client
                .reserve_le_lease(&accepted_device, &accepted_owner, 11)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        publisher.release_name("org.bluez").await.unwrap();
        let (resource, replacement) = dbus_tokio::connection::new_session_sync().unwrap();
        let replacement_task = tokio::spawn(resource);
        replacement
            .request_name("org.bluez", false, false, false)
            .await
            .unwrap();
        let reply = pending
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .method_return()
            .append1(7_u64);
        publisher.send(reply).unwrap();
        assert_eq!(
            accepted.await.unwrap().unwrap(),
            7,
            "accepted original-owner reservation must remain identifiable"
        );
        let owner_error = client
            .connect_le_lease(&device, &original_owner, 7)
            .await
            .expect_err("retired owner cannot admit a new connection effect");
        assert!(
            owner_error.to_string().contains(
                "the bound BlueZ daemon owner changed; create a fresh manager to resolve and verify native authority"
            ),
            "owner replacement must direct recovery to a fresh manager, not a copied attestation: {owner_error}"
        );
        let receipt = client
            .release_le_lease(&device, &original_owner, 7, None)
            .await
            .unwrap();
        assert_eq!(receipt.scope, LeLeaseReleaseScope::ReservationReleased);
        assert_eq!(
            client
                .recover_le_lease(&device, &original_owner, 11)
                .await
                .unwrap(),
            Some(7)
        );
        client_task.abort();
        publisher_task.abort();
        replacement_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires a dedicated dbus-run-session; native client proof only"]
    async fn private_bus_release_scope_is_not_a_physical_disconnect_boolean() {
        use dbus::channel::{MatchingReceiver, Sender};
        use dbus::message::MatchRule;
        use std::sync::{Arc, Mutex};
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let (resource, publisher) = dbus_tokio::connection::new_session_sync().unwrap();
        let publisher_task = tokio::spawn(resource);
        publisher
            .request_name("org.bluez", false, false, false)
            .await
            .unwrap();
        let scope = Arc::new(Mutex::new("physical-released"));
        let observed_scope = scope.clone();
        publisher.start_receive(
            MatchRule::new_method_call(),
            Box::new(move |message, connection| {
                assert_eq!(message.interface().as_deref(), Some(INTERFACE));
                let reply = match message.member().as_deref() {
                    Some("ReserveLease") => {
                        assert_eq!(message.path().as_deref(), Some("/org/bluez/hci0"));
                        assert_eq!(
                            message.read1::<dbus::Path<'static>>().unwrap().to_string(),
                            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
                        );
                        message.method_return().append1(7_u64)
                    }
                    Some("ConnectLease") => {
                        assert_eq!(message.read1::<u64>().unwrap(), 7);
                        message.method_return().append1(9_u64)
                    }
                    Some("ReleaseLease") => {
                        assert_eq!(message.read1::<u64>().unwrap(), 7);
                        let scope = observed_scope.lock().unwrap().to_string();
                        let has_reason = scope == "physical-released";
                        message
                            .method_return()
                            .append3(3_u32, 7_u64, 9_u64)
                            .append1(scope)
                            .append2(has_reason, if has_reason { 2_u8 } else { 0_u8 })
                    }
                    Some("AckLease") => {
                        assert_eq!(message.read1::<u64>().unwrap(), 7);
                        message.method_return()
                    }
                    Some("RecoverLease") => {
                        assert_eq!(message.read1::<u64>().unwrap(), 12);
                        message.method_return().append1(0_u64)
                    }
                    _ => panic!("unexpected method; no stock Connect/Disconnect fallback"),
                };
                connection.send(reply).unwrap();
                true
            }),
        );
        let (resource, connection) = dbus_tokio::connection::new_session_sync().unwrap();
        let client_task = tokio::spawn(resource);
        let client = BluetoothSession::from_connection(connection);
        let owner = publisher.unique_name().to_string();
        let device = DeviceId::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(
            client.reserve_le_lease(&device, &owner, 11).await.unwrap(),
            7
        );
        assert_eq!(
            client.connect_le_lease(&device, &owner, 7).await.unwrap(),
            9
        );
        for (wire, expected) in [
            ("physical-released", LeLeaseReleaseScope::PhysicalReleased),
            (
                "lease-released-protected",
                LeLeaseReleaseScope::LeaseReleasedProtected,
            ),
            (
                "lease-released-indeterminate",
                LeLeaseReleaseScope::LeaseReleasedIndeterminate,
            ),
        ] {
            *scope.lock().unwrap() = wire;
            assert_eq!(
                client
                    .release_le_lease(&device, &owner, 7, Some(9))
                    .await
                    .unwrap()
                    .scope,
                expected
            );
        }
        *scope.lock().unwrap() = "released";
        assert_eq!(
            client.recover_le_lease(&device, &owner, 12).await.unwrap(),
            None
        );
        client
            .acknowledge_le_lease(&device, &owner, 7)
            .await
            .unwrap();
        assert_eq!(client.allocate_le_reservation_id().unwrap(), 1);
        assert_eq!(client.clone().allocate_le_reservation_id().unwrap(), 2);
        assert_eq!(
            client
                .scoped_match_cleanup()
                .allocate_le_reservation_id()
                .unwrap(),
            3
        );
        assert!(
            client
                .release_le_lease(&device, &owner, 7, Some(9))
                .await
                .is_err()
        );
        client_task.abort();
        publisher_task.abort();
    }
}
