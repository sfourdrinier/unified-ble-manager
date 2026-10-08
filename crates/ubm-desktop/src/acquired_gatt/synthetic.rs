//! Deterministic acquired transport boundary. It never opens a production
//! radio or substitutes for unavailable native acquisition.
use super::{AcquiredGattIo, AcquiredGattTransport, AcquisitionKind, TransportFuture};
use crate::{boundary::InstanceKey, errors::DesktopError};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

#[derive(Debug, Clone, Copy)]
pub struct SyntheticAcquisition {
    pub write: bool,
    pub notify: bool,
    pub mtu: u16,
}
#[derive(Debug, Default)]
struct FactoryState {
    configurations: HashMap<String, SyntheticAcquisition>,
    owners: HashMap<(InstanceKey, AcquisitionKind), Weak<Session>>,
    blocked: bool,
    writes: Vec<Vec<u8>>,
}
#[derive(Debug, Default, Clone)]
pub struct SyntheticAcquiredGatt {
    inner: Arc<Mutex<FactoryState>>,
}
#[derive(Debug)]
struct SessionState {
    terminal: Option<DesktopError>,
    packets: VecDeque<Vec<u8>>,
}
#[derive(Debug)]
struct Session {
    state: Mutex<SessionState>,
    changed: Notify,
    factory: Weak<Mutex<FactoryState>>,
    kind: AcquisitionKind,
    maximum: usize,
}
#[derive(Debug)]
struct Io {
    session: Arc<Session>,
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poisoned| {
        eprintln!("synthetic acquired transport ownership mutex poisoned");
        poisoned.into_inner()
    })
}
fn closed() -> DesktopError {
    DesktopError::new(
        BleErrorCode::StreamClosed,
        BleErrorDomain::Stream,
        "gatt.acquired",
    )
}
impl SyntheticAcquiredGatt {
    pub fn configure(
        &self,
        peer: &str,
        configuration: SyntheticAcquisition,
    ) -> Result<(), DesktopError> {
        if !(23..=517).contains(&configuration.mtu) {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "synthetic.acquire.mtu",
            ));
        }
        let mut state = lock(&self.inner);
        if !state.configurations.contains_key(peer) && state.configurations.len() >= 4096 {
            return Err(DesktopError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "synthetic.acquire.configurations",
            ));
        }
        state.configurations.insert(peer.into(), configuration);
        Ok(())
    }
    pub fn acquire(
        &self,
        scope: &InstanceKey,
        kind: AcquisitionKind,
    ) -> Result<AcquiredGattTransport, DesktopError> {
        let mut state = lock(&self.inner);
        state.owners.retain(|_, owner| {
            owner
                .upgrade()
                .is_some_and(|session| lock(&session.state).terminal.is_none())
        });
        let configuration = state.configurations.get(&scope.0).ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "gatt.acquire",
            )
        })?;
        let available = match kind {
            AcquisitionKind::Write => configuration.write,
            AcquisitionKind::Notify => configuration.notify,
        };
        if !available {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "gatt.acquire",
            ));
        }
        if state.owners.contains_key(&(scope.clone(), kind)) {
            return Err(DesktopError::new(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.acquire",
            ));
        }
        if state.owners.len() >= 256 {
            return Err(DesktopError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "gatt.acquire",
            ));
        }
        let mtu = configuration.mtu;
        let session = Arc::new(Session {
            state: Mutex::new(SessionState {
                terminal: None,
                packets: VecDeque::new(),
            }),
            changed: Notify::new(),
            factory: Arc::downgrade(&self.inner),
            kind,
            maximum: usize::from(mtu - 3).min(512),
        });
        state
            .owners
            .insert((scope.clone(), kind), Arc::downgrade(&session));
        Ok(AcquiredGattTransport {
            mtu,
            io: Arc::new(Io { session }),
        })
    }
    fn sessions(&self) -> Vec<Arc<Session>> {
        lock(&self.inner)
            .owners
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }
    pub fn block(&self, blocked: bool) {
        lock(&self.inner).blocked = blocked;
        for session in self.sessions() {
            session.changed.notify_waiters();
        }
    }
    pub fn writes(&self) -> Vec<Vec<u8>> {
        lock(&self.inner).writes.clone()
    }
    pub fn active(&self) -> usize {
        self.sessions()
            .iter()
            .filter(|session| lock(&session.state).terminal.is_none())
            .count()
    }
    pub fn notify(&self, bytes: Vec<u8>) {
        for session in self
            .sessions()
            .into_iter()
            .filter(|session| session.kind == AcquisitionKind::Notify)
        {
            let mut state = lock(&session.state);
            if state.terminal.is_some() {
                continue;
            }
            if bytes.len() > session.maximum {
                state.terminal = Some(DesktopError::new(
                    BleErrorCode::BytesTooLarge,
                    BleErrorDomain::Gatt,
                    "gatt.acquired-notify",
                ));
            } else if state.packets.len() >= 64 {
                state.terminal = Some(DesktopError::new(
                    BleErrorCode::StreamOverflow,
                    BleErrorDomain::Stream,
                    "gatt.acquired-notify",
                ));
            } else {
                state.packets.push_back(bytes.clone());
            }
            drop(state);
            session.changed.notify_waiters();
        }
    }
    pub fn hup(&self) {
        for session in self.sessions() {
            lock(&session.state).terminal = Some(
                DesktopError::new(
                    BleErrorCode::PlatformTransport,
                    BleErrorDomain::Platform,
                    "gatt.acquired",
                )
                .with_platform(crate::errors::PlatformDetail::new(
                    "linux-acquired-fd",
                    "hup",
                )),
            );
            session.changed.notify_waiters();
        }
    }
}
impl AcquiredGattIo for Io {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> TransportFuture<'a, ()> {
        Box::pin(async move {
            if self.session.kind != AcquisitionKind::Write {
                return Err(DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.acquired-write",
                ));
            }
            if bytes.len() > self.session.maximum {
                return Err(DesktopError::new(
                    BleErrorCode::BytesTooLarge,
                    BleErrorDomain::Gatt,
                    "gatt.acquired-write",
                ));
            }
            loop {
                let notified = self.session.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let factory = self.session.factory.upgrade().ok_or_else(closed)?;
                // Factory then session is the ownership lock order, matching acquisition.
                let sent = {
                    let mut factory = lock(&factory);
                    let state = lock(&self.session.state);
                    if let Some(error) = &state.terminal {
                        return Err(error.clone());
                    }
                    if factory.blocked {
                        false
                    } else {
                        if factory.writes.len() >= 4096 {
                            return Err(DesktopError::new(
                                BleErrorCode::StreamQuota,
                                BleErrorDomain::Stream,
                                "synthetic.acquired-write-log",
                            ));
                        }
                        factory.writes.push(bytes.to_vec());
                        true
                    }
                };
                if sent {
                    return Ok(());
                }
                notified.await;
            }
        })
    }
    fn receive(&self) -> TransportFuture<'_, Vec<u8>> {
        Box::pin(async move {
            if self.session.kind != AcquisitionKind::Notify {
                return Err(DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.acquired-notify",
                ));
            }
            loop {
                let notified = self.session.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut state = lock(&self.session.state);
                    if let Some(error) = &state.terminal {
                        return Err(error.clone());
                    }
                    if let Some(packet) = state.packets.pop_front() {
                        if packet.len() > self.session.maximum {
                            return Err(DesktopError::new(
                                BleErrorCode::BytesTooLarge,
                                BleErrorDomain::Gatt,
                                "gatt.acquired-notify",
                            ));
                        }
                        return Ok(packet);
                    }
                }
                notified.await;
            }
        })
    }
    fn close(&self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            let mut state = lock(&self.session.state);
            if state.terminal.is_none() {
                state.terminal = Some(closed());
            }
            state.packets.clear();
            drop(state);
            self.session.changed.notify_waiters();
            Ok(())
        })
    }
}
