//! Production acquisition over an isolated D-Bus with real packet descriptors.
//! These controls do not establish bluetoothd or physical-radio qualification.
use super::*;
use crate::acquired_gatt::{AcquiredGattIo, AcquisitionKind, LinuxAcquiredGattIo};
use std::os::fd::{FromRawFd, OwnedFd};

const SERVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0001";
const CHAR_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0001/char0002";
const SERVICE_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const CHAR_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";

struct FixtureObjects {
    flags: Vec<String>,
}
#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FixtureObjects {
    fn get_managed_objects(&self) -> zbus::fdo::Result<Managed> {
        let service = HashMap::from([(
            "UUID".into(),
            OwnedValue::try_from(Value::from(SERVICE_UUID))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
        )]);
        let characteristic = HashMap::from([
            (
                "UUID".into(),
                OwnedValue::try_from(Value::from(CHAR_UUID))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            ),
            (
                "Service".into(),
                OwnedValue::from(ObjectPath::try_from(SERVICE_PATH).unwrap()),
            ),
            (
                "Flags".into(),
                OwnedValue::try_from(Value::from(self.flags.clone()))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            ),
        ]);
        Ok(HashMap::from([
            (
                OwnedObjectPath::try_from(SERVICE_PATH).unwrap(),
                HashMap::from([(SERVICE.into(), service)]),
            ),
            (
                OwnedObjectPath::try_from(CHAR_PATH).unwrap(),
                HashMap::from([(CHARACTERISTIC.into(), characteristic)]),
            ),
        ]))
    }
}

#[derive(Default)]
struct FixtureState {
    descriptor: StdMutex<Option<OwnedFd>>,
    sender: StdMutex<Option<String>>,
    entered: tokio::sync::Notify,
    unblock: tokio::sync::Notify,
    held: bool,
    mtu: std::sync::atomic::AtomicU16,
}
struct FixtureCharacteristic {
    state: Arc<FixtureState>,
}
#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl FixtureCharacteristic {
    #[zbus(property)]
    fn write_acquired(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn notify_acquired(&self) -> bool {
        false
    }
    async fn acquire_write(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<(zbus::zvariant::OwnedFd, u16)> {
        *self.state.sender.lock().unwrap() = Some(header.sender().unwrap().to_string());
        self.state.entered.notify_one();
        if self.state.held {
            self.state.unblock.notified().await;
        }
        let fd = self
            .state
            .descriptor
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("fixture FD already acquired".into()))?;
        Ok((
            fd.into(),
            self.state.mtu.load(std::sync::atomic::Ordering::Acquire),
        ))
    }
}

fn pair() -> (OwnedFd, OwnedFd) {
    let mut descriptors = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                descriptors.as_mut_ptr(),
            )
        },
        0
    );
    unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    }
}
async fn fixture(
    held: bool,
    flags: Vec<String>,
) -> (zbus::Connection, Arc<FixtureState>, OwnedFd, Arc<Bluez>) {
    assert_eq!(
        std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
        Ok("1")
    );
    let (fd, peer) = pair();
    let state = Arc::new(FixtureState {
        descriptor: StdMutex::new(Some(fd)),
        held,
        mtu: std::sync::atomic::AtomicU16::new(23),
        ..FixtureState::default()
    });
    let publisher = zbus::connection::Builder::session()
        .unwrap()
        .name(BLUEZ)
        .unwrap()
        .serve_at("/", FixtureObjects { flags })
        .unwrap()
        .serve_at(
            CHAR_PATH,
            FixtureCharacteristic {
                state: state.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let authority = Bluez::open_authority("hci0", crate::boundary::BluezBus::Session, None)
        .await
        .unwrap();
    (publisher, state, peer, authority)
}
fn scope() -> InstanceKey {
    (
        "hci0/dev_AA_BB_CC_DD_EE_FF".into(),
        SERVICE_UUID.into(),
        0,
        CHAR_UUID.into(),
        0,
    )
}
async fn sender_exists(connection: &zbus::Connection, name: &str) -> bool {
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(name,),
        )
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; real FD, no system Bluetooth access"]
async fn private_bus_acquired_fd_owns_a_dedicated_sender_and_closes_both() {
    let (publisher, state, peer, authority) =
        fixture(false, vec!["write-without-response".into()]).await;
    let transport = authority
        .acquire_gatt(&scope(), AcquisitionKind::Write)
        .await
        .unwrap();
    assert_eq!(transport.mtu, 23);
    let sender = state.sender.lock().unwrap().clone().unwrap();
    assert_ne!(sender, authority.conn.unique_name().unwrap().as_str());
    assert!(sender_exists(&publisher, &sender).await);
    let receiver = LinuxAcquiredGattIo::new(peer, 20).unwrap();
    transport.io.send(&[42]).await.unwrap();
    assert_eq!(receiver.receive().await.unwrap(), vec![42]);
    transport.io.close().await.unwrap();
    assert!(!sender_exists(&publisher, &sender).await);
    assert_eq!(
        receiver.receive().await.unwrap_err().code(),
        BleErrorCode::PlatformTransport
    );
    receiver.close().await.unwrap();
    assert!(authority.finish_acquired().await.is_empty());
    publisher.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; cancellation before the FD reply"]
async fn private_bus_cancelled_acquisition_retires_its_pending_sender() {
    let (publisher, state, peer, authority) =
        fixture(true, vec!["write-without-response".into()]).await;
    let owner = authority.clone();
    let worker =
        tokio::spawn(async move { owner.acquire_gatt(&scope(), AcquisitionKind::Write).await });
    tokio::time::timeout(Duration::from_secs(1), state.entered.notified())
        .await
        .unwrap();
    let sender = state.sender.lock().unwrap().clone().unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(authority.finish_acquired().await.is_empty());
    assert!(!sender_exists(&publisher, &sender).await);
    state.unblock.notify_one();
    drop(peer);
    publisher.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; optional method and flags admission"]
async fn private_bus_absent_method_and_ineligible_flags_are_explicit_without_fallback() {
    let (publisher, state, peer, authority) = fixture(false, vec!["notify".into()]).await;
    assert_eq!(
        authority
            .acquire_gatt(&scope(), AcquisitionKind::Write)
            .await
            .unwrap_err()
            .code(),
        BleErrorCode::CapabilityUnsupported
    );
    assert!(state.sender.lock().unwrap().is_none());
    assert_eq!(
        authority
            .acquire_gatt(&scope(), AcquisitionKind::Notify)
            .await
            .unwrap_err()
            .code(),
        BleErrorCode::CapabilityUnsupported
    );
    assert!(authority.finish_acquired().await.is_empty());
    assert!(state.descriptor.lock().unwrap().is_some());
    drop(peer);
    publisher.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; invalid native MTU compensation"]
async fn private_bus_invalid_mtu_retires_the_fd_and_dedicated_sender() {
    for mtu in [0, 22, 518, u16::MAX] {
        let (publisher, state, peer, authority) =
            fixture(false, vec!["write-without-response".into()]).await;
        state.mtu.store(mtu, std::sync::atomic::Ordering::Release);
        let error = authority
            .acquire_gatt(&scope(), AcquisitionKind::Write)
            .await
            .unwrap_err();
        assert_eq!(error.code(), BleErrorCode::ProtocolViolation);
        assert!(authority.finish_acquired().await.is_empty());
        let sender = state.sender.lock().unwrap().clone().unwrap();
        assert!(!sender_exists(&publisher, &sender).await);
        let receiver = LinuxAcquiredGattIo::new(peer, 20).unwrap();
        assert_eq!(
            receiver.receive().await.unwrap_err().code(),
            BleErrorCode::PlatformTransport
        );
        receiver.close().await.unwrap();
        publisher.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; daemon owner changes before deferred publication"]
async fn private_bus_daemon_replacement_cannot_publish_the_old_owners_deferred_fd() {
    let (publisher, state, peer, authority) =
        fixture(true, vec!["write-without-response".into()]).await;
    let old = authority.clone();
    let opening =
        tokio::spawn(async move { old.acquire_gatt(&scope(), AcquisitionKind::Write).await });
    tokio::time::timeout(Duration::from_secs(1), state.entered.notified())
        .await
        .unwrap();
    let sender = state.sender.lock().unwrap().clone().unwrap();
    publisher.release_name(BLUEZ).await.unwrap();
    let replacement = zbus::Connection::session().await.unwrap();
    replacement.request_name(BLUEZ).await.unwrap();
    state.unblock.notify_one();
    let error = opening.await.unwrap().unwrap_err();
    assert_eq!(error.code(), BleErrorCode::BackendReset);
    assert!(authority.finish_acquired().await.is_empty());
    assert!(!sender_exists(&replacement, &sender).await);
    let receiver = LinuxAcquiredGattIo::new(peer, 20).unwrap();
    assert_eq!(
        receiver.receive().await.unwrap_err().code(),
        BleErrorCode::PlatformTransport
    );
    receiver.close().await.unwrap();
    replacement.close().await.unwrap();
    publisher.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session; daemon name vanishes before deferred publication"]
async fn private_bus_daemon_absence_cannot_publish_a_deferred_fd() {
    let (publisher, state, peer, authority) =
        fixture(true, vec!["write-without-response".into()]).await;
    let old = authority.clone();
    let opening =
        tokio::spawn(async move { old.acquire_gatt(&scope(), AcquisitionKind::Write).await });
    tokio::time::timeout(Duration::from_secs(1), state.entered.notified())
        .await
        .unwrap();
    let sender = state.sender.lock().unwrap().clone().unwrap();
    publisher.release_name(BLUEZ).await.unwrap();
    state.unblock.notify_one();
    let error = opening.await.unwrap().unwrap_err();
    assert_eq!(error.code(), BleErrorCode::BackendReset);
    assert!(error.platform().is_some());
    assert!(authority.finish_acquired().await.is_empty());
    assert!(!sender_exists(&publisher, &sender).await);
    let receiver = LinuxAcquiredGattIo::new(peer, 20).unwrap();
    assert_eq!(
        receiver.receive().await.unwrap_err().code(),
        BleErrorCode::PlatformTransport
    );
    receiver.close().await.unwrap();
    publisher.close().await.unwrap();
}
