//! Owned optional acquired-GATT byte transports. An acquired FD is an OS
//! resource, never an ordinary WriteValue/StartNotify fallback.
use crate::errors::DesktopError;
use std::{future::Future, pin::Pin, sync::Arc};

pub type TransportFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DesktopError>> + Send + 'a>>;

/// Exact transport selected by an explicit caller acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcquisitionKind {
    Write,
    Notify,
}

/// The owned transport's byte boundary. Sending a packet is one operation;
/// implementations preserve packet boundaries and close wakes every waiter.
pub trait AcquiredGattIo: Send + Sync + std::fmt::Debug {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> TransportFuture<'a, ()>;
    fn receive(&self) -> TransportFuture<'_, Vec<u8>>;
    fn close(&self) -> TransportFuture<'_, ()>;
}

#[derive(Debug, Clone)]
pub struct AcquiredGattTransport {
    /// MTU from this acquisition's native answer, never from a requested MTU.
    pub mtu: u16,
    pub io: Arc<dyn AcquiredGattIo>,
}

pub(crate) mod ownership;
pub mod synthetic;

#[cfg(all(target_os = "linux", feature = "btleplug"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "btleplug"))]
pub use linux::LinuxAcquiredGattIo;

#[cfg(all(test, target_os = "linux", feature = "btleplug"))]
mod tests {
    use super::*;
    use std::{
        os::fd::{FromRawFd, OwnedFd},
        time::Duration,
    };

    fn socket_pair() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        // The test owns both descriptors only after socketpair succeeds.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                descriptors.as_mut_ptr(),
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    #[tokio::test]
    async fn seqpacket_transport_preserves_empty_payload_and_distinguishes_hup() {
        let (first, second) = socket_pair();
        let writer = LinuxAcquiredGattIo::new(first, 20).unwrap();
        let reader = LinuxAcquiredGattIo::new(second, 20).unwrap();
        writer.send(&[]).await.unwrap();
        assert_eq!(reader.receive().await.unwrap(), Vec::<u8>::new());
        writer.send(&[42]).await.unwrap();
        assert_eq!(reader.receive().await.unwrap(), vec![42]);
        writer.close().await.unwrap();
        let failure = tokio::time::timeout(Duration::from_secs(1), reader.receive())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            failure.code(),
            ubm_core::contracts::BleErrorCode::PlatformTransport
        );
        reader.close().await.unwrap();
    }

    #[tokio::test]
    async fn close_wakes_a_held_receive_and_releases_every_descriptor_reference() {
        let (first, second) = socket_pair();
        let reader = Arc::new(LinuxAcquiredGattIo::new(first, 20).unwrap());
        let reading = tokio::spawn({
            let reader = reader.clone();
            async move { reader.receive().await }
        });
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_secs(1), reader.close())
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), reading)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        reader.close().await.unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn cancelled_close_attempt_retains_a_retry_owner() {
        use futures_util::poll;
        let (first, second) = socket_pair();
        let reader = LinuxAcquiredGattIo::new(first, 20).unwrap();
        let mut receiving = reader.receive();
        assert!(poll!(receiving.as_mut()).is_pending());
        let mut closing = reader.close();
        assert!(poll!(closing.as_mut()).is_pending());
        drop(closing);
        assert!(poll!(receiving.as_mut()).is_ready());
        drop(receiving);
        tokio::time::timeout(Duration::from_secs(1), reader.close())
            .await
            .unwrap()
            .unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn oversized_packet_and_failed_send_remain_explicit_faults() {
        let (first, second) = socket_pair();
        let writer = LinuxAcquiredGattIo::new(first, 20).unwrap();
        let reader = LinuxAcquiredGattIo::new(second, 1).unwrap();
        writer.send(&[1, 2]).await.unwrap();
        assert_eq!(
            reader.receive().await.unwrap_err().code(),
            ubm_core::contracts::BleErrorCode::BytesTooLarge
        );
        reader.close().await.unwrap();
        assert_eq!(
            writer.send(&[3]).await.unwrap_err().code(),
            ubm_core::contracts::BleErrorCode::PlatformTransport
        );
        writer.close().await.unwrap();
    }
}
