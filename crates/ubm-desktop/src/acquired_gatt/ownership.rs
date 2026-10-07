//! Bounded parent/lease ownership for pending and published acquisitions.
use super::{AcquiredGattTransport, AcquisitionKind};
use crate::errors::DesktopError;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

fn error(code: BleErrorCode) -> DesktopError {
    DesktopError::new(code, BleErrorDomain::Gatt, "gatt.acquired")
}

pub(crate) struct Registry {
    prefix: String,
    next: AtomicU64,
    rows: Mutex<HashMap<String, Arc<Resource>>>,
}
pub(crate) struct Resource {
    pub handle: String,
    pub peer: String,
    pub lease: Option<String>,
    pub kind: AcquisitionKind,
    pub admission: OnceLock<Admission>,
    state: Mutex<State>,
    wake: Notify,
    cleanup: tokio::sync::Mutex<()>,
}
struct State {
    transport: Option<AcquiredGattTransport>,
    terminal: Option<DesktopError>,
    closed: bool,
}
pub(crate) struct Admission {
    pub scope: crate::boundary::InstanceKey,
    pub selector: ubm_core::central::PathSelector,
    pub connection: Option<String>,
    pub database: Option<String>,
}
pub(crate) struct Opening {
    registry: Arc<Registry>,
    pub resource: Arc<Resource>,
    published: bool,
}
impl Registry {
    pub fn assert_available(
        &self,
        scope: &crate::boundary::InstanceKey,
        kind: AcquisitionKind,
    ) -> Result<(), DesktopError> {
        let rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rows.values().any(|row| {
            row.kind == kind
                && row
                    .admission
                    .get()
                    .is_some_and(|admission| &admission.scope == scope)
                && !row
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .closed
        }) {
            return Err(error(BleErrorCode::OwnershipDenied)
                .with_detail("the characteristic's acquired transport is still owned"));
        }
        Ok(())
    }
    pub fn new(ordinal: u64) -> Arc<Self> {
        Arc::new(Self {
            prefix: format!("acquired-{ordinal}-"),
            next: AtomicU64::new(1),
            rows: Mutex::new(HashMap::new()),
        })
    }
    pub fn reserve(
        self: &Arc<Self>,
        peer: &str,
        lease: Option<&str>,
        kind: AcquisitionKind,
    ) -> Result<Opening, DesktopError> {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rows.len() >= 256 {
            return Err(error(BleErrorCode::StreamQuota).with_detail(
                "acquired handle capacity includes pending and unreleased terminal handles",
            ));
        }
        let id = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| error(BleErrorCode::StreamQuota))?;
        let handle = format!("{}{id}", self.prefix);
        let resource = Arc::new(Resource {
            handle: handle.clone(),
            peer: peer.into(),
            lease: lease.map(str::to_owned),
            kind,
            admission: OnceLock::new(),
            state: Mutex::new(State {
                transport: None,
                terminal: None,
                closed: false,
            }),
            wake: Notify::new(),
            cleanup: tokio::sync::Mutex::new(()),
        });
        rows.insert(handle, resource.clone());
        Ok(Opening {
            registry: self.clone(),
            resource,
            published: false,
        })
    }
    fn valid_handle(&self, handle: &str) -> bool {
        handle
            .strip_prefix(&self.prefix)
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| {
                id > 0
                    && id < self.next.load(Ordering::Relaxed)
                    && handle == format!("{}{id}", self.prefix)
            })
    }
    pub fn lookup(&self, handle: &str) -> Result<Arc<Resource>, DesktopError> {
        if !self.valid_handle(handle) {
            return Err(error(BleErrorCode::OwnershipDenied));
        }
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(handle)
            .cloned()
            .ok_or_else(|| error(BleErrorCode::StreamClosed))
    }
    pub async fn close(&self, handle: &str) -> Result<(), DesktopError> {
        if !self.valid_handle(handle) {
            return Err(error(BleErrorCode::OwnershipDenied));
        }
        let resource = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(handle)
            .cloned();
        if let Some(resource) = resource {
            resource.retire(error(BleErrorCode::StreamClosed));
            resource.close_transport().await?;
            self.rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(handle);
        }
        Ok(())
    }
    pub fn retire(&self, peer: Option<&str>, lease: Option<&str>, cause: DesktopError) {
        let rows: Vec<_> = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|row| {
                peer.is_none_or(|peer| row.peer == peer)
                    && lease.is_none_or(|lease| row.lease.as_deref() == Some(lease))
            })
            .cloned()
            .collect();
        for row in rows {
            row.retire(cause.clone());
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = row.close_transport().await {
                        eprintln!("ubm-desktop: acquired transport cleanup retained: {error}");
                    }
                });
            }
        }
    }
    pub async fn release_scope(
        &self,
        peer: Option<&str>,
        lease: Option<&str>,
    ) -> Vec<DesktopError> {
        let handles: Vec<_> = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|row| {
                peer.is_none_or(|peer| row.peer == peer)
                    && lease.is_none_or(|lease| row.lease.as_deref() == Some(lease))
            })
            .map(|row| row.handle.clone())
            .collect();
        let mut failures = Vec::new();
        for handle in handles {
            if let Err(error) = self.close(&handle).await {
                failures.push(error);
            }
        }
        failures
    }
    pub fn counts(&self) -> (usize, usize) {
        let rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = rows
            .values()
            .filter(|row| {
                let state = row
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.transport.is_some() && !state.closed
            })
            .count();
        let pending = rows
            .values()
            .filter(|row| {
                let state = row
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.transport.is_none() && !state.closed
            })
            .count();
        (active, pending)
    }
}
impl Opening {
    pub async fn publish(
        mut self,
        transport: AcquiredGattTransport,
    ) -> Result<Arc<Resource>, DesktopError> {
        let terminal = {
            let mut state = self
                .resource
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.transport = Some(transport);
            state.terminal.clone()
        };
        if let Some(error) = terminal {
            return match self.resource.close_transport().await {
                Ok(()) => Err(error),
                Err(cleanup) => match crate::errors::cleanup_result(
                    "abandoned-acquired-gatt",
                    vec![error, cleanup],
                ) {
                    Err(error) => Err(error),
                    Ok(()) => unreachable!("nonempty failures"),
                },
            };
        }
        self.published = true;
        Ok(self.resource.clone())
    }
}
impl Drop for Opening {
    fn drop(&mut self) {
        if !self.published {
            self.resource.retire(error(BleErrorCode::StreamClosed));
            let has_transport = self
                .resource
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .transport
                .is_some();
            if has_transport {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let registry = self.registry.clone();
                    let row = self.resource.clone();
                    runtime.spawn(async move {
                        if let Err(error) = registry.close(&row.handle).await {
                            eprintln!(
                                "ubm-desktop: abandoned acquired handle cleanup retained: {error}"
                            );
                        }
                    });
                }
            } else {
                self.registry
                    .rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&self.resource.handle);
            }
        }
    }
}
impl Resource {
    pub fn check(&self) -> Result<(), DesktopError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal
            .clone()
            .map_or(Ok(()), Err)
    }
    pub fn transport(&self) -> Result<AcquiredGattTransport, DesktopError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        state
            .transport
            .clone()
            .ok_or_else(|| error(BleErrorCode::StreamClosed))
    }
    pub fn retire(&self, error: DesktopError) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal.is_none() {
            state.terminal = Some(error);
        }
        drop(state);
        self.wake.notify_waiters();
    }
    pub async fn ended(&self) -> DesktopError {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if let Err(error) = self.check() {
                return error;
            }
            wake.await;
        }
    }
    async fn close_transport(&self) -> Result<(), DesktopError> {
        let _gate = self.cleanup.lock().await;
        let transport = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return Ok(());
            }
            state.transport.clone()
        };
        if let Some(transport) = transport {
            transport.io.close().await?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.transport = None;
            state.closed = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retirement_wakes_openings_and_preserves_the_original_terminal() {
        let registry = Registry::new(7);
        let opening = registry
            .reserve("peer", Some("lease"), AcquisitionKind::Write)
            .unwrap();
        let resource = opening.resource.clone();
        registry.retire(
            Some("peer"),
            Some("lease"),
            error(BleErrorCode::GattStaleHandle),
        );
        assert_eq!(resource.ended().await.code(), BleErrorCode::GattStaleHandle);
        assert_eq!(
            resource.check().unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        drop(opening);
        assert_eq!(registry.counts(), (0, 0));
    }
    #[test]
    fn parent_scope_capacity_and_abandoned_openings_have_no_history() {
        let registry = Registry::new(8);
        let mut openings = Vec::new();
        for _ in 0..256 {
            openings.push(
                registry
                    .reserve("peer", None, AcquisitionKind::Notify)
                    .unwrap(),
            );
        }
        assert_eq!(
            registry
                .reserve("other", None, AcquisitionKind::Write)
                .err()
                .unwrap()
                .code(),
            BleErrorCode::StreamQuota
        );
        assert!(
            Registry::new(9)
                .lookup(&openings[0].resource.handle)
                .is_err()
        );
        drop(openings);
        for _ in 0..1024 {
            drop(
                registry
                    .reserve("peer", None, AcquisitionKind::Write)
                    .unwrap(),
            );
        }
        assert_eq!(registry.counts(), (0, 0));
    }
    #[tokio::test]
    async fn lease_retirement_does_not_end_another_owners_transport() {
        let registry = Registry::new(10);
        let first = registry
            .reserve("peer", Some("first"), AcquisitionKind::Write)
            .unwrap();
        let second = registry
            .reserve("peer", Some("second"), AcquisitionKind::Notify)
            .unwrap();
        registry.retire(
            Some("peer"),
            Some("first"),
            error(BleErrorCode::OperationDisconnected),
        );
        assert!(first.resource.check().is_err());
        assert!(second.resource.check().is_ok());
    }
}
