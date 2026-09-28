//! UBM-private authoritative LE GATT observation; never inferred from cache.
use crate::{BluetoothError, BluetoothSession, DeviceId};
use dbus::arg::{RefArg, cast};
use dbus::nonblock::Proxy;
use std::collections::VecDeque;

const INTERFACE: &str = "org.unifiedblemanager.LEGatt1";
const POLL_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);
/// Total authoritative snapshot admission bound, including every D-Bus await.
pub const LE_GATT_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

macro_rules! vocabulary {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            fn parse(value: &str) -> Result<Self, BluetoothError> {
                match value { $($wire => Ok(Self::$variant)),+, _ => Err(protocol(format!("unknown {} value {value:?}",stringify!($name)))) }
            }
        }
    };
}

vocabulary!(LeGattBearer { None => "none", Le => "le", Bredr => "bredr", Mixed => "mixed", Unknown => "unknown" });
vocabulary!(LeGattStatus { Disconnected => "disconnected", Discovering => "discovering", Ready => "ready", Failed => "failed", Unsupported => "unsupported" });
vocabulary!(LeGattErrorStage { None => "none", Transport => "transport", Discovery => "discovery", Projection => "projection", Bearer => "bearer", Generation => "generation", Policy => "policy", Registration => "registration" });

/// Daemon-local identity. It is not an address, durable database ID or lease.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeGattReadyToken {
    pub daemon_owner: String,
    pub attachment: u64,
    pub revision: u64,
}

/// The exact version-1 daemon answer, including failures and pending state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeGattSnapshot {
    pub daemon_owner: String,
    pub version: u32,
    pub attachment: u64,
    pub revision: u64,
    pub bearer: LeGattBearer,
    pub status: LeGattStatus,
    pub error_stage: LeGattErrorStage,
    pub errno: i32,
    pub att_error: u8,
}

impl LeGattSnapshot {
    pub fn ready_token(&self) -> Result<LeGattReadyToken, BluetoothError> {
        self.validate()?;
        if self.status != LeGattStatus::Ready {
            return Err(BluetoothError::LeGattNotReady(Box::new(self.clone())));
        }
        Ok(LeGattReadyToken {
            daemon_owner: self.daemon_owner.clone(),
            attachment: self.attachment,
            revision: self.revision,
        })
    }

    fn validate(&self) -> Result<(), BluetoothError> {
        if dbus::strings::BusName::new(&self.daemon_owner).is_err()
            || !self.daemon_owner.starts_with(':')
        {
            return Err(protocol(
                "snapshot requires a valid pinned unique daemon owner".into(),
            ));
        }
        if self.version != 1 {
            return Err(BluetoothError::LeGattUnsupportedVersion(self.version));
        }
        if self.errno < 0 {
            return Err(protocol(
                "snapshot errno must be nonnegative native errno".into(),
            ));
        }
        match self.status {
            LeGattStatus::Ready | LeGattStatus::Discovering => {
                if self.bearer != LeGattBearer::Le
                    || self.attachment == 0
                    || self.revision == 0
                    || self.error_stage != LeGattErrorStage::None
                    || self.errno != 0
                    || self.att_error != 0
                {
                    return Err(protocol(
                        "ready/discovering requires current LE attachment/revision and no errors"
                            .into(),
                    ));
                }
            }
            LeGattStatus::Failed | LeGattStatus::Unsupported => {
                if self.errno == 0 || self.error_stage == LeGattErrorStage::None {
                    return Err(protocol(
                        "failed/unsupported requires actual errno and error stage".into(),
                    ));
                }
            }
            LeGattStatus::Disconnected => {
                if self.errno == 0
                    && (self.error_stage != LeGattErrorStage::None || self.att_error != 0)
                {
                    return Err(protocol(
                        "error-free disconnected snapshot must have no error detail".into(),
                    ));
                }
                if self.errno > 0 && self.error_stage == LeGattErrorStage::None {
                    return Err(protocol(
                        "disconnected errno requires actual error stage".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn protocol(reason: String) -> BluetoothError {
    BluetoothError::LeGattProtocolError(reason)
}

fn parse_snapshot(
    owner: &str,
    raw: VecDeque<Box<dyn RefArg>>,
) -> Result<LeGattSnapshot, BluetoothError> {
    if raw.len() != 8 {
        return Err(protocol(
            "GetSnapshot requires exactly uttsssiy, eight fields".into(),
        ));
    }
    let bad_type = || protocol("GetSnapshot requires exact uttsssiy field types".into());
    let version = *cast::<u32>(&raw[0]).ok_or_else(bad_type)?;
    let snapshot = LeGattSnapshot {
        daemon_owner: owner.to_owned(),
        version,
        attachment: *cast::<u64>(&raw[1]).ok_or_else(bad_type)?,
        revision: *cast::<u64>(&raw[2]).ok_or_else(bad_type)?,
        bearer: LeGattBearer::parse(cast::<String>(&raw[3]).ok_or_else(bad_type)?)?,
        status: LeGattStatus::parse(cast::<String>(&raw[4]).ok_or_else(bad_type)?)?,
        error_stage: LeGattErrorStage::parse(cast::<String>(&raw[5]).ok_or_else(bad_type)?)?,
        errno: *cast::<i32>(&raw[6]).ok_or_else(bad_type)?,
        att_error: *cast::<u8>(&raw[7]).ok_or_else(bad_type)?,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

impl BluetoothSession {
    /// Nonallocating attestation lookup; no fallback or automatic attestation.
    pub fn attested_le_owner(&self) -> Option<&str> {
        self.destination
            .starts_with(':')
            .then_some(self.destination.as_str())
    }

    /// Read the private daemon answer. Missing API is an explicit unsupported
    /// mechanism, not readiness from Device1.ServicesResolved or exported cache.
    pub async fn le_gatt_snapshot(
        &self,
        device: &DeviceId,
        owner: &str,
    ) -> Result<LeGattSnapshot, BluetoothError> {
        self.le_owner(owner).await?;
        let proxy = Proxy::new(
            owner,
            device.object_path.clone(),
            crate::DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let raw: VecDeque<Box<dyn RefArg>> =
            match proxy.method_call(INTERFACE, "GetSnapshot", ()).await {
                Ok(raw) => raw,
                Err(error)
                    if matches!(
                        error.name(),
                        Some(
                            "org.freedesktop.DBus.Error.UnknownMethod"
                                | "org.freedesktop.DBus.Error.UnknownInterface"
                        )
                    ) =>
                {
                    return Err(BluetoothError::LeGattApiUnsupported(error));
                }
                Err(error) => return Err(error.into()),
            };
        self.le_owner(owner).await?;
        parse_snapshot(owner, raw)
    }

    /// Initial admission only. The total five-second deadline includes every
    /// D-Bus await, and only authoritative DISCOVERING is polled at 100 ms.
    pub async fn await_le_gatt_ready(
        &self,
        device: &DeviceId,
        owner: &str,
    ) -> Result<LeGattReadyToken, BluetoothError> {
        self.await_le_gatt_ready_with_timeout(device, owner, LE_GATT_OBSERVATION_TIMEOUT)
            .await
    }

    async fn await_le_gatt_ready_with_timeout(
        &self,
        device: &DeviceId,
        owner: &str,
        deadline: std::time::Duration,
    ) -> Result<LeGattReadyToken, BluetoothError> {
        tokio::time::timeout(deadline, async {
            loop {
                let snapshot = self.le_gatt_snapshot(device, owner).await?;
                if snapshot.status != LeGattStatus::Discovering {
                    return snapshot.ready_token();
                }
                tokio::time::sleep(POLL_PERIOD).await;
            }
        })
        .await
        .unwrap_or(Err(BluetoothError::LeGattObservationTimedOut(deadline)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::too_many_arguments,
        reason = "exact eight-field protocol fixture"
    )]
    fn wire(
        version: u32,
        attachment: u64,
        revision: u64,
        bearer: &str,
        status: &str,
        stage: &str,
        errno: i32,
        att: u8,
    ) -> VecDeque<Box<dyn RefArg>> {
        let mut message =
            dbus::Message::new_signal("/device", "org.example.Test", "Snapshot").unwrap();
        message.append_all((
            version, attachment, revision, bearer, status, stage, errno, att,
        ));
        message.read_all().unwrap()
    }

    #[test]
    fn ready_token_requires_exact_authoritative_version_and_transport() {
        let raw = wire(1, 7, 9, "le", "ready", "none", 0, 0);
        let snapshot = parse_snapshot(":1.5", raw).unwrap();
        assert_eq!(
            snapshot.ready_token().unwrap(),
            LeGattReadyToken {
                daemon_owner: ":1.5".into(),
                attachment: 7,
                revision: 9
            }
        );
        for raw in [
            wire(2, 7, 9, "le", "ready", "none", 0, 0),
            wire(1, 0, 9, "le", "ready", "none", 0, 0),
            wire(1, 7, 0, "le", "ready", "none", 0, 0),
            wire(1, 7, 9, "bredr", "ready", "none", 0, 0),
            wire(1, 7, 9, "mixed", "ready", "none", 0, 0),
            wire(1, 7, 9, "unknown", "ready", "none", 0, 0),
            wire(1, 7, 9, "le", "ready", "discovery", 5, 0),
            wire(1, 7, 9, "le", "ready", "none", 0, 10),
        ] {
            assert!(parse_snapshot(":1.5", raw).is_err());
        }
    }

    #[test]
    fn strict_wire_rejects_missing_extra_wrong_types_and_unknown_vocabulary() {
        let mut extra = wire(1, 7, 9, "le", "ready", "none", 0, 0);
        extra.push_back(Box::new(1u32));
        assert!(parse_snapshot(":1.5", extra).is_err());
        let mut short = wire(1, 7, 9, "le", "ready", "none", 0, 0);
        short.pop_back();
        assert!(parse_snapshot(":1.5", short).is_err());
        let mut wrong = wire(1, 7, 9, "le", "ready", "none", 0, 0);
        wrong[0] = Box::new(1u64);
        assert!(parse_snapshot(":1.5", wrong).is_err());
        for raw in [
            wire(1, 7, 9, "future", "ready", "none", 0, 0),
            wire(1, 7, 9, "le", "future", "none", 0, 0),
            wire(1, 7, 9, "le", "failed", "future", 5, 10),
            wire(1, 7, 9, "le", "failed", "discovery", -5, 10),
            wire(1, 7, 9, "le", "failed", "none", 0, 10),
        ] {
            assert!(parse_snapshot(":1.5", raw).is_err());
        }
    }

    #[test]
    fn non_ready_unknown_bearer_preserves_actual_failure_and_retirement() {
        let snapshot = parse_snapshot(
            ":1.5",
            wire(1, 7, 9, "unknown", "failed", "generation", 75, 0),
        )
        .unwrap();
        assert!(
            matches!(snapshot.ready_token(), Err(BluetoothError::LeGattNotReady(actual))
            if actual.bearer == LeGattBearer::Unknown && actual.error_stage == LeGattErrorStage::Generation
                && actual.errno == 75 && actual.att_error == 0)
        );
        let retired = parse_snapshot(
            ":1.5",
            wire(1, 7, 10, "unknown", "disconnected", "none", 0, 0),
        )
        .unwrap();
        assert!(matches!(
            retired.ready_token(),
            Err(BluetoothError::LeGattNotReady(_))
        ));
    }

    #[test]
    fn actual_failed_snapshot_retains_stage_errno_and_att_answer() {
        let snapshot =
            parse_snapshot(":1.5", wire(1, 7, 9, "le", "failed", "projection", 5, 10)).unwrap();
        assert_eq!(snapshot.error_stage, LeGattErrorStage::Projection);
        assert_eq!(snapshot.errno, 5);
        assert_eq!(snapshot.att_error, 10);
        assert!(matches!(
            snapshot.ready_token(),
            Err(BluetoothError::LeGattNotReady(_))
        ));
        for stage in ["policy", "registration"] {
            assert!(parse_snapshot(":1.5", wire(1, 7, 9, "le", "failed", stage, 95, 0)).is_ok());
        }
        assert!(
            parse_snapshot(
                ":1.5",
                wire(1, 7, 9, "unknown", "unsupported", "bearer", 95, 0)
            )
            .unwrap()
            .ready_token()
            .is_err()
        );
    }
}
