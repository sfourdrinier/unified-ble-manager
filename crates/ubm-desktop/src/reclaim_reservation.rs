// crates/ubm-desktop/src/reclaim_reservation.rs
//! Bounded, replayable recorder handoff reservation for trusted native hosts.

use crate::{
    boundary::RadioBoundary,
    central::{DesktopCentral, OrderedRecordingReclaimPermit},
    errors::DesktopError,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

fn error(detail: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Cleanup,
        "gatt.ordered.reclaim-reservation",
    )
    .with_detail(detail)
}

fn denied() -> DesktopError {
    DesktopError::new(
        BleErrorCode::OwnershipDenied,
        BleErrorDomain::Gatt,
        "gatt.ordered.reclaim-reservation",
    )
}

#[derive(Clone, Debug)]
pub enum ReclaimReservationStatus {
    Reserved,
    Pending,
    Ready,
    Committed,
    Releasing {
        mapping_committed: bool,
    },
    ReleaseFailed {
        mapping_committed: bool,
        error: DesktopError,
    },
    Released {
        mapping_committed: bool,
    },
    Retired {
        mapping_committed: bool,
    },
    Failed(DesktopError),
}

struct Entry {
    id: u64,
    capability: String,
    caller_key: String,
    lease: String,
    status: ReclaimReservationStatus,
    permit: Option<OrderedRecordingReclaimPermit>,
    cancelled: bool,
    published: bool,
    gate_held: bool,
}

pub(crate) struct ReclaimReservationState {
    next: u64,
    retired: u64,
    retired_owner: Option<(String, String, String)>,
    retired_mapping_committed: bool,
    retired_gate_held: bool,
    retirement_acknowledged: bool,
    retirement_forgotten: bool,
    authority: Option<String>,
    revoked: bool,
    entry: Option<Entry>,
}

impl ReclaimReservationState {
    pub(crate) fn new() -> Self {
        Self {
            next: 1,
            retired: 0,
            retired_owner: None,
            retired_mapping_committed: false,
            retired_gate_held: false,
            retirement_acknowledged: true,
            retirement_forgotten: true,
            authority: None,
            revoked: false,
            entry: None,
        }
    }
    pub(crate) fn revoke(&mut self) {
        self.authority = None;
        self.revoked = true;
    }
}

/// All handles for one recording share this state; host authenticates the
/// capability before forwarding an operation to this internal Rust API.
pub struct ReclaimReservationOwner<B: RadioBoundary> {
    central: DesktopCentral<B>,
    peer: String,
    collection: u64,
    token: u64,
    state: Arc<Mutex<ReclaimReservationState>>,
    gate: Arc<AtomicU8>,
    runtime: tokio::runtime::Handle,
}

impl<B: RadioBoundary> ReclaimReservationOwner<B> {
    pub(crate) fn new(
        central: DesktopCentral<B>,
        peer: String,
        collection: u64,
        token: u64,
        state: Arc<Mutex<ReclaimReservationState>>,
        gate: Arc<AtomicU8>,
    ) -> Self {
        Self {
            central,
            peer,
            collection,
            token,
            state,
            gate,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// Bind the recorder-validated secret once; later factories must match it.
    pub fn bind_capability(&self, capability: &str) -> Result<(), DesktopError> {
        if capability.is_empty() || capability.len() > 512 {
            return Err(denied());
        }
        let mut state = self.state.lock().expect("reclaim reservation state");
        if state.revoked || self.gate.load(Ordering::Acquire) == 2 {
            return Err(denied());
        }
        match &state.authority {
            Some(bound) if bound != capability => Err(denied()),
            Some(_) => Ok(()),
            None => {
                state.authority = Some(capability.to_owned());
                Ok(())
            }
        }
    }

    /// Close effect authority and supervise cleanup of the exact retained
    /// reservation. Receipts remain authenticated and available for replay.
    pub(crate) fn revoke_and_cleanup(&self) {
        let release = {
            let mut state = self.state.lock().expect("reclaim reservation state");
            state.revoke();
            let Some(entry) = state.entry.as_mut() else {
                return;
            };
            match entry.status {
                ReclaimReservationStatus::Reserved => {
                    entry.cancelled = true;
                    entry.status = ReclaimReservationStatus::Failed(DesktopError::new(
                        BleErrorCode::OperationAborted,
                        BleErrorDomain::Cleanup,
                        "gatt.ordered.reclaim-reservation",
                    ));
                    if entry.gate_held {
                        let _ =
                            self.gate
                                .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
                        entry.gate_held = false;
                    }
                    None
                }
                ReclaimReservationStatus::Pending => {
                    entry.cancelled = true;
                    None
                }
                ReclaimReservationStatus::Ready => {
                    entry.cancelled = true;
                    match entry.permit.take() {
                        Some(permit) => {
                            entry.status = ReclaimReservationStatus::Releasing {
                                mapping_committed: false,
                            };
                            Some((entry.id, permit))
                        }
                        None => {
                            entry.status = ReclaimReservationStatus::ReleaseFailed {
                                mapping_committed: false,
                                error: error("ready reclaim permit missing during revocation"),
                            };
                            None
                        }
                    }
                }
                _ => None,
            }
        };
        if let Some((id, permit)) = release {
            drive_release(
                &self.runtime,
                Arc::clone(&self.state),
                Arc::clone(&self.gate),
                id,
                permit,
            );
        }
    }

    /// Return the next request ID only to the recorder capability holder.
    pub fn next_request_id(&self, capability: &str) -> Result<u64, DesktopError> {
        let state = self.state.lock().expect("reclaim reservation state");
        if state.authority.as_deref() != Some(capability) {
            return Err(denied());
        }
        if state.entry.is_some() || !state.retirement_forgotten {
            return Err(error("previous reclaim receipt awaits acknowledgement"));
        }
        Ok(state.next)
    }

    /// A monotonically numbered host request is idempotent while retained.
    pub fn reserve(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        if capability.is_empty()
            || lease.is_empty()
            || caller_key.is_empty()
            || capability.len() > 512
            || lease.len() > 512
            || caller_key.len() > 512
        {
            return Err(error("invalid capability or lease"));
        }
        let mut state = self.state.lock().expect("reclaim reservation state");
        if state.authority.as_deref() != Some(capability) {
            return Err(denied());
        }
        if let Some(entry) = &state.entry {
            return if entry.id == id
                && entry.capability == capability
                && entry.lease == lease
                && entry.caller_key == caller_key
            {
                Ok(())
            } else if entry.capability != capability {
                Err(denied())
            } else {
                Err(error("another reclaim is retained"))
            };
        }
        if !state.retirement_forgotten {
            return Err(error("previous reclaim receipt awaits forget"));
        }
        if id != state.next {
            return Err(error("stale or out-of-sequence reclaim request"));
        }
        let next = id
            .checked_add(1)
            .ok_or_else(|| error("reclaim request IDs exhausted"))?;
        self.gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| error("group is closing or another operation is retained"))?;
        state.next = next;
        state.entry = Some(Entry {
            id,
            capability: capability.to_owned(),
            caller_key: caller_key.to_owned(),
            lease: lease.to_owned(),
            status: ReclaimReservationStatus::Reserved,
            permit: None,
            cancelled: false,
            published: false,
            gate_held: true,
        });
        Ok(())
    }

    pub fn start(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let lease = {
            let mut state = self.state.lock().expect("reclaim reservation state");
            if state.authority.as_deref() != Some(capability) {
                return Err(denied());
            }
            let entry = state
                .entry
                .as_mut()
                .filter(|e| {
                    e.id == id
                        && e.capability == capability
                        && e.lease == lease
                        && e.caller_key == caller_key
                })
                .ok_or_else(|| denied())?;
            if !matches!(entry.status, ReclaimReservationStatus::Reserved) {
                return Ok(());
            }
            entry.status = ReclaimReservationStatus::Pending;
            entry.lease.clone()
        };
        let central = self.central.clone();
        let peer = self.peer.clone();
        let collection = self.collection;
        let token = self.token;
        let state = Arc::clone(&self.state);
        let gate = Arc::clone(&self.gate);
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            let result = central
                .acquire_ordered_recording_reclaim(&peer, collection, token, &lease)
                .await;
            let release = {
                let mut state = state.lock().expect("reclaim reservation state");
                match state.entry.as_mut().filter(|e| e.id == id) {
                    Some(entry) => match result {
                        Ok(permit) if entry.cancelled => {
                            entry.status = ReclaimReservationStatus::Releasing {
                                mapping_committed: entry.published,
                            };
                            Some(permit)
                        }
                        Ok(permit) => {
                            entry.permit = Some(permit);
                            entry.status = ReclaimReservationStatus::Ready;
                            None
                        }
                        Err(error) => {
                            entry.status = ReclaimReservationStatus::Failed(error);
                            None
                        }
                    },
                    None => None,
                }
            };
            if let Some(permit) = release {
                drive_release(&runtime, state, gate, id, permit);
            }
        });
        Ok(())
    }

    pub fn status(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<ReclaimReservationStatus, DesktopError> {
        let state = self.state.lock().expect("reclaim reservation state");
        if let Some(entry) = state.entry.as_ref().filter(|e| {
            e.id == id
                && e.capability == capability
                && e.lease == lease
                && e.caller_key == caller_key
        }) {
            return Ok(entry.status.clone());
        }
        if id == state.retired
            && state
                .retired_owner
                .as_ref()
                .is_some_and(|(cap, held_lease, key)| {
                    cap == capability && held_lease == lease && key == caller_key
                })
        {
            return Ok(ReclaimReservationStatus::Retired {
                mapping_committed: state.retired_mapping_committed,
            });
        }
        Err(denied())
    }

    /// Runs only an in-process host mapping mutation. Lock order is this
    /// reservation then the host mapping; callers must not invert it. The
    /// closure must never invoke user code, await, or reenter this owner.
    pub fn commit(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
        publish: impl FnOnce() -> Result<(), DesktopError>,
    ) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("reclaim reservation state");
        let authorized = state.authority.as_deref() == Some(capability);
        let entry = state
            .entry
            .as_mut()
            .filter(|e| {
                e.id == id
                    && e.capability == capability
                    && e.lease == lease
                    && e.caller_key == caller_key
            })
            .ok_or_else(|| denied())?;
        if entry.published {
            return Ok(());
        }
        if !authorized {
            return Err(denied());
        }
        match entry.status {
            ReclaimReservationStatus::Ready => {
                if entry.permit.is_none() {
                    return Err(error("reclaim permit missing"));
                }
                publish()?;
                entry.published = true;
                let permit = entry.permit.take().expect("checked reclaim permit");
                entry.status = ReclaimReservationStatus::Releasing {
                    mapping_committed: true,
                };
                drop(state);
                drive_release(
                    &self.runtime,
                    Arc::clone(&self.state),
                    Arc::clone(&self.gate),
                    id,
                    permit,
                );
                Ok(())
            }
            _ => Err(error("reclaim is not ready to publish")),
        }
    }

    /// Cancel before publication. A pending native acquisition remains owned
    /// until its exact permit is released or its no-effect result is known.
    pub fn cancel(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let permit = {
            let mut state = self.state.lock().expect("reclaim reservation state");
            let entry = state
                .entry
                .as_mut()
                .filter(|e| {
                    e.id == id
                        && e.capability == capability
                        && e.lease == lease
                        && e.caller_key == caller_key
                })
                .ok_or_else(|| denied())?;
            match entry.status {
                ReclaimReservationStatus::Reserved => {
                    entry.status = ReclaimReservationStatus::Failed(DesktopError::new(
                        BleErrorCode::OperationAborted,
                        BleErrorDomain::Cleanup,
                        "gatt.ordered.reclaim-reservation",
                    ));
                    if entry.gate_held {
                        let _ =
                            self.gate
                                .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
                        entry.gate_held = false;
                    }
                    None
                }
                ReclaimReservationStatus::Pending => {
                    entry.cancelled = true;
                    None
                }
                ReclaimReservationStatus::Ready => {
                    let permit = entry
                        .permit
                        .take()
                        .ok_or_else(|| error("reclaim permit missing"))?;
                    entry.status = ReclaimReservationStatus::Releasing {
                        mapping_committed: entry.published,
                    };
                    Some(permit)
                }
                ReclaimReservationStatus::Releasing { .. }
                | ReclaimReservationStatus::ReleaseFailed { .. }
                | ReclaimReservationStatus::Released { .. }
                | ReclaimReservationStatus::Retired { .. }
                | ReclaimReservationStatus::Failed(_) => None,
                ReclaimReservationStatus::Committed => {
                    return Err(error("committed reclaim cannot be cancelled"));
                }
            }
        };
        if let Some(permit) = permit {
            drive_release(
                &self.runtime,
                Arc::clone(&self.state),
                Arc::clone(&self.gate),
                id,
                permit,
            );
        }
        Ok(())
    }

    /// Retains the permit and retries release independently of the waiter.
    pub fn release(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let permit = {
            let mut state = self.state.lock().expect("reclaim reservation state");
            let entry = state
                .entry
                .as_mut()
                .filter(|e| {
                    e.id == id
                        && e.capability == capability
                        && e.lease == lease
                        && e.caller_key == caller_key
                })
                .ok_or_else(|| denied())?;
            match entry.status {
                ReclaimReservationStatus::Releasing { .. }
                | ReclaimReservationStatus::ReleaseFailed { .. }
                | ReclaimReservationStatus::Released { .. } => return Ok(()),
                ReclaimReservationStatus::Committed => {}
                _ => return Err(error("reclaim has not been committed")),
            }
            let permit = entry
                .permit
                .take()
                .ok_or_else(|| error("reclaim permit missing"))?;
            entry.status = ReclaimReservationStatus::Releasing {
                mapping_committed: entry.published,
            };
            permit
        };
        drive_release(
            &self.runtime,
            Arc::clone(&self.state),
            Arc::clone(&self.gate),
            id,
            permit,
        );
        Ok(())
    }

    pub fn retire(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("reclaim reservation state");
        if id == state.retired
            && state
                .retired_owner
                .as_ref()
                .is_some_and(|(cap, held_lease, key)| {
                    cap == capability && held_lease == lease && key == caller_key
                })
        {
            return Ok(());
        }
        if id <= state.retired {
            return Err(denied());
        }
        let entry = state
            .entry
            .as_ref()
            .filter(|e| {
                e.id == id
                    && e.capability == capability
                    && e.lease == lease
                    && e.caller_key == caller_key
            })
            .ok_or_else(|| denied())?;
        if !matches!(
            entry.status,
            ReclaimReservationStatus::Released { .. } | ReclaimReservationStatus::Failed(_)
        ) {
            return Err(error("reclaim still owns cleanup"));
        }
        let (published, gate_held) = (entry.published, entry.gate_held);
        state.retired_mapping_committed = published;
        state.retired_gate_held = gate_held;
        state.entry = None;
        state.retired = id;
        state.retirement_acknowledged = false;
        state.retirement_forgotten = false;
        state.retired_owner = Some((
            capability.to_owned(),
            lease.to_owned(),
            caller_key.to_owned(),
        ));
        Ok(())
    }

    /// The owning host acknowledges receipt of retire before issuing a new
    /// request. Exact acknowledgement retries replay without releasing data.
    pub fn acknowledge_retirement(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("reclaim reservation state");
        if id != state.retired
            || !state
                .retired_owner
                .as_ref()
                .is_some_and(|(cap, held_lease, key)| {
                    cap == capability && held_lease == lease && key == caller_key
                })
        {
            return Err(denied());
        }
        state.retirement_acknowledged = true;
        Ok(())
    }

    /// Retire an acknowledged receipt. The trusted host must retain its own
    /// authenticated replay receipt across supervisor revocation before reply.
    pub fn forget_retirement(
        &self,
        id: u64,
        capability: &str,
        lease: &str,
        caller_key: &str,
    ) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("reclaim reservation state");
        if id != state.retired
            || !state
                .retired_owner
                .as_ref()
                .is_some_and(|(cap, held_lease, key)| {
                    cap == capability && held_lease == lease && key == caller_key
                })
        {
            return Err(denied());
        }
        if !state.retirement_acknowledged {
            return Err(error("reclaim retirement is not acknowledged"));
        }
        if !state.retirement_forgotten {
            state.retirement_forgotten = true;
            if state.retired_gate_held {
                let _ = self
                    .gate
                    .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
                state.retired_gate_held = false;
            }
        }
        Ok(())
    }
}

fn drive_release(
    runtime: &tokio::runtime::Handle,
    state: Arc<Mutex<ReclaimReservationState>>,
    gate: Arc<AtomicU8>,
    id: u64,
    mut permit: OrderedRecordingReclaimPermit,
) {
    runtime.spawn(async move {
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            let result = permit.release().await;
            {
                let mut state = state.lock().expect("reclaim reservation state");
                let Some(entry) = state.entry.as_mut().filter(|e| e.id == id) else {
                    return;
                };
                match result {
                    Ok(()) => {
                        if entry.gate_held {
                            let _ =
                                gate.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
                            entry.gate_held = false;
                        }
                        entry.status = ReclaimReservationStatus::Released {
                            mapping_committed: entry.published,
                        };
                        return;
                    }
                    Err(error) => {
                        entry.status = ReclaimReservationStatus::ReleaseFailed {
                            mapping_committed: entry.published,
                            error,
                        }
                    }
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(30));
        }
    });
}
