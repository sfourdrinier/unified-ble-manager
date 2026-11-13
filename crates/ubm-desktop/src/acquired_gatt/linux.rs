//! Linux packet-FD ownership. SO_PASSCRED distinguishes a valid empty packet
//! from EOF: Unix seqpacket recvmsg returns zero for both, but only a packet
//! carries ancillary credentials. Credentials are never exposed or retained.
use super::{AcquiredGattIo, TransportFuture};
use crate::errors::{DesktopError, PlatformDetail};
use std::{
    io,
    os::fd::{AsRawFd, IntoRawFd, OwnedFd},
    sync::{Arc, Mutex},
};
use tokio::{io::unix::AsyncFd, sync::Notify};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

#[derive(Debug)]
struct State {
    fd: Option<Arc<AsyncFd<OwnedFd>>>,
    active: usize,
    closing: bool,
    close_in_progress: bool,
    closed: Option<Result<(), DesktopError>>,
}
#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    changed: Notify,
    closing: Notify,
    maximum_payload: usize,
}
#[derive(Debug, Clone)]
pub struct LinuxAcquiredGattIo {
    inner: Arc<Inner>,
}

fn failure(operation: &str, error: io::Error) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformTransport,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(error.to_string())
    .with_platform(
        PlatformDetail::new(
            "linux-acquired-fd",
            error
                .raw_os_error()
                .map_or_else(|| "io".into(), |code| code.to_string()),
        )
        .with_message(error.to_string()),
    )
}
fn terminal(operation: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::StreamClosed,
        BleErrorDomain::Stream,
        operation,
    )
    .with_detail("the acquired transport is closed")
}
fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.state.lock().unwrap_or_else(|poisoned| {
        eprintln!("acquired FD ownership mutex was poisoned; retaining cleanup authority");
        poisoned.into_inner()
    })
}

struct Use {
    fd: Option<Arc<AsyncFd<OwnedFd>>>,
    inner: Arc<Inner>,
}
impl Use {
    fn fd(&self) -> &AsyncFd<OwnedFd> {
        self.fd.as_ref().expect("owned active FD")
    }
}
impl Drop for Use {
    fn drop(&mut self) {
        drop(self.fd.take());
        state(&self.inner).active -= 1;
        self.inner.changed.notify_waiters();
    }
}

struct CloseAttempt {
    inner: Arc<Inner>,
    armed: bool,
}
impl Drop for CloseAttempt {
    fn drop(&mut self) {
        if self.armed {
            state(&self.inner).close_in_progress = false;
            self.inner.changed.notify_waiters();
        }
    }
}

impl LinuxAcquiredGattIo {
    pub fn new(fd: OwnedFd, maximum_payload: usize) -> Result<Self, DesktopError> {
        if maximum_payload == 0 || maximum_payload > 512 {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolViolation,
                BleErrorDomain::Gatt,
                "gatt.acquire-fd",
            )
            .with_detail("invalid acquired ATT payload bound"));
        }
        let enabled: libc::c_int = 1;
        // The owned FD remains alive; the option is a copied integer.
        let result = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                std::ptr::from_ref(&enabled).cast(),
                std::mem::size_of_val(&enabled)
                    .try_into()
                    .expect("integer option fits socklen"),
            )
        };
        if result != 0 {
            return Err(failure(
                "gatt.acquire-fd.passcred",
                io::Error::last_os_error(),
            ));
        }
        // BlueZ delivers a nonblocking socket. Fail closed if this descriptor
        // is not nonblocking rather than blocking the shared executor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(failure("gatt.acquire-fd.flags", io::Error::last_os_error()));
        }
        if flags & libc::O_NONBLOCK == 0 {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolViolation,
                BleErrorDomain::Gatt,
                "gatt.acquire-fd.flags",
            )
            .with_detail("acquired socket is blocking"));
        }
        let fd = AsyncFd::new(fd).map_err(|error| failure("gatt.acquire-fd.register", error))?;
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    fd: Some(Arc::new(fd)),
                    active: 0,
                    closing: false,
                    close_in_progress: false,
                    closed: None,
                }),
                changed: Notify::new(),
                closing: Notify::new(),
                maximum_payload,
            }),
        })
    }

    fn acquire(&self, operation: &str) -> Result<Use, DesktopError> {
        let mut state = state(&self.inner);
        if state.closing {
            return Err(terminal(operation));
        }
        let fd = state.fd.clone().ok_or_else(|| terminal(operation))?;
        state.active += 1;
        Ok(Use {
            fd: Some(fd),
            inner: self.inner.clone(),
        })
    }

    async fn until_closed(&self) {
        loop {
            let notified = self.inner.closing.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if state(&self.inner).closing {
                return;
            }
            notified.await;
        }
    }
}

enum Packet {
    Value(Vec<u8>),
    Oversized,
    End,
}
fn receive_packet(fd: &OwnedFd, maximum: usize) -> io::Result<Packet> {
    let mut bytes = vec![0u8; maximum];
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    #[repr(C)]
    struct Control {
        alignment: libc::cmsghdr,
        extra: [u8; 64],
    }
    // Zeroed C message headers/buffer contain no invalid Rust values.
    let mut control: Control = unsafe { std::mem::zeroed() };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = std::ptr::from_mut(&mut control).cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let received = unsafe {
        libc::recvmsg(
            fd.as_raw_fd(),
            &mut message,
            libc::MSG_TRUNC | libc::MSG_DONTWAIT,
        )
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "acquired packet ancillary data was truncated",
        ));
    }
    let mut credentials = false;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        let current = unsafe { &*header };
        credentials |=
            current.cmsg_level == libc::SOL_SOCKET && current.cmsg_type == libc::SCM_CREDENTIALS;
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    if received == 0 && !credentials {
        return Ok(Packet::End);
    }
    let length = usize::try_from(received)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative packet length"))?;
    if length > maximum || message.msg_flags & libc::MSG_TRUNC != 0 {
        return Ok(Packet::Oversized);
    }
    bytes.truncate(length);
    Ok(Packet::Value(bytes))
}

impl AcquiredGattIo for LinuxAcquiredGattIo {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> TransportFuture<'a, ()> {
        Box::pin(async move {
            const OP: &str = "gatt.acquired-write";
            if bytes.len() > self.inner.maximum_payload {
                return Err(DesktopError::new(
                    BleErrorCode::BytesTooLarge,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            let owned = self.acquire(OP)?;
            loop {
                let mut ready = tokio::select! { biased; _ = self.until_closed() => return Err(terminal(OP)), ready = owned.fd().writable() => ready.map_err(|error| failure(OP, error))? };
                match ready.try_io(|fd| {
                    let written = unsafe {
                        libc::send(
                            fd.get_ref().as_raw_fd(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                        )
                    };
                    if written < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if usize::try_from(written).ok() != Some(bytes.len()) {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "partial acquired packet write",
                        ));
                    }
                    Ok(())
                }) {
                    Ok(result) => return result.map_err(|error| failure(OP, error)),
                    Err(_) => continue,
                }
            }
        })
    }
    fn receive(&self) -> TransportFuture<'_, Vec<u8>> {
        Box::pin(async move {
            const OP: &str = "gatt.acquired-notify";
            let owned = self.acquire(OP)?;
            loop {
                let mut ready = tokio::select! { biased; _ = self.until_closed() => return Err(terminal(OP)), ready = owned.fd().readable() => ready.map_err(|error| failure(OP, error))? };
                match ready.try_io(|fd| receive_packet(fd.get_ref(), self.inner.maximum_payload)) {
                    Ok(Ok(Packet::Value(bytes))) => return Ok(bytes),
                    Ok(Ok(Packet::Oversized)) => {
                        return Err(DesktopError::new(
                            BleErrorCode::BytesTooLarge,
                            BleErrorDomain::Gatt,
                            OP,
                        )
                        .with_detail(
                            "acquired notification exceeds the actual ATT payload bound",
                        ));
                    }
                    Ok(Ok(Packet::End)) => {
                        return Err(DesktopError::new(
                            BleErrorCode::PlatformTransport,
                            BleErrorDomain::Platform,
                            OP,
                        )
                        .with_detail("acquired socket peer closed (HUP)")
                        .with_platform(PlatformDetail::new("linux-acquired-fd", "hup")));
                    }
                    Ok(Err(error)) => return Err(failure(OP, error)),
                    Err(_) => continue,
                }
            }
        })
    }
    fn close(&self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            let first = {
                let mut state = state(&self.inner);
                let first =
                    !state.close_in_progress && (state.closed.is_none() || state.fd.is_some());
                state.closing = true;
                if first {
                    state.close_in_progress = true;
                }
                first
            };
            self.inner.closing.notify_waiters();
            if first {
                let mut attempt = CloseAttempt {
                    inner: self.inner.clone(),
                    armed: true,
                };
                let fd = loop {
                    let notified = self.inner.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    {
                        let mut state = state(&self.inner);
                        if state.active == 0 {
                            break state.fd.take();
                        }
                    }
                    notified.await;
                };
                let result = match fd {
                    Some(fd) => match Arc::try_unwrap(fd) {
                        Ok(fd) => {
                            let raw = fd.into_inner().into_raw_fd();
                            let result = unsafe { libc::close(raw) };
                            if result == 0 {
                                Ok(())
                            } else {
                                Err(failure("gatt.acquired-close", io::Error::last_os_error()))
                            }
                        }
                        Err(fd) => {
                            // Retain unexpected ownership rather than lose the descriptor.
                            state(&self.inner).fd = Some(fd);
                            Err(DesktopError::new(
                                BleErrorCode::LifecycleInvariantViolation,
                                BleErrorDomain::Cleanup,
                                "gatt.acquired-close",
                            )
                            .with_detail("unaccounted acquired FD reference"))
                        }
                    },
                    None => Ok(()),
                };
                {
                    let mut state = state(&self.inner);
                    state.closed = Some(result.clone());
                    state.close_in_progress = false;
                }
                attempt.armed = false;
                self.inner.changed.notify_waiters();
                result
            } else {
                loop {
                    let notified = self.inner.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let progress = {
                        let state = state(&self.inner);
                        (
                            state.close_in_progress,
                            state.closed.clone(),
                            state.fd.is_some(),
                        )
                    };
                    if !progress.0 {
                        if progress.1.is_some() && !progress.2 {
                            // Linux releases the descriptor early in close,
                            // including error returns. Never retry its numeric
                            // fd: it may already belong to another resource.
                            // The first attempt delivered the original error.
                            return Ok(());
                        }
                        if let Some(result) = progress.1 {
                            return result;
                        }
                        return self.close().await;
                    }
                    notified.await;
                }
            }
        })
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;
    #[tokio::test]
    async fn retry_after_reported_close_failure_never_closes_a_reused_descriptor() {
        // The production close syscall has consumed the descriptor before
        // reporting an advisory Linux close error. The original caller keeps
        // that error; a later cleanup sees the resource already released.
        let original = failure(
            "gatt.acquired-close",
            io::Error::from_raw_os_error(libc::EIO),
        );
        let transport = LinuxAcquiredGattIo {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    fd: None,
                    active: 0,
                    closing: true,
                    close_in_progress: false,
                    closed: Some(Err(original.clone())),
                }),
                changed: Notify::new(),
                closing: Notify::new(),
                maximum_payload: 20,
            }),
        };
        assert_eq!(
            state(&transport.inner)
                .closed
                .clone()
                .unwrap()
                .unwrap_err()
                .platform(),
            original.platform()
        );
        transport.close().await.unwrap();
        transport.close().await.unwrap();
        assert!(state(&transport.inner).fd.is_none());
        assert!(transport.send(&[1]).await.is_err());
    }
}
