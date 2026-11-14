//! Standalone reclaim-v3 receipt model. Nothing in this module is wired to a
//! radio, bridge, native graph permit, or the production recorder mapping.
//!
//! The caller must authenticate the host/session/runtime and recorder capability
//! before invoking this core, and serialize its *real* graph and mapping changes
//! with the same linearization boundary. No external work may run under the
//! mutex. Acquisition attempts must be registered before dispatch, and their
//! exact completion reconciled even after fencing. Cleanup updates represent
//! actual release results, never a timeout. This model performs no native work.
//! Incarnations are host-issued, strictly increasing, and never reused during
//! this core's lifetime. Teardown is authenticated and must fence all old work;
//! after core destruction the host must ensure old messages cannot reach a new
//! core. Sequences are contiguous and reclaim-only per runtime and recorder;
//! gaps must be fenced before that particular window can advance. Generic
//! `Retired` after compaction proves only absence below the high-water mark,
//! not identity or acknowledgement; clients must not treat it as an exact ack.
//! A contradictory or unknown acquisition completion is a protocol fault and
//! may itself represent a held native resource. Its owner must supervise it
//! outside this model; rejecting it here does not dispose of that resource.
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard};

const MAX_FIELD_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Runtime {
    pub host: String,
    pub session: String,
    pub runtime: String,
    pub incarnation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Recorder {
    pub host: String,
    pub peer: String,
    pub group: String,
    pub recording: String,
    pub capability: String,
    pub incarnation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Graph {
    pub connection: u64,
    pub database: u64,
    pub owner: String,
    pub attachment: String,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub runtime: Runtime,
    pub sequence: u64,
    pub request_id: String,
    pub recorder: Recorder,
    pub lease: String,
    pub expected_owner: String,
    pub expected_mapping: u64,
    pub expected_graph: Graph,
    pub expected_authority: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reject {
    Authority,
    Graph,
    Owner,
    Mapping,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Pending,
    Committed { graph: Graph, authority: u64 },
    FencedNoEffect,
    Rejected(Reject),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cleanup {
    NotRequired,
    Pending,
    Failed,
    Released,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcquisitionDispatch {
    Dispatch,
    AlreadyRegistered,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Acquisition {
    NotStarted,
    Acquiring { attempt: String },
    Ready { attempt: String, permit: String },
    Failed { attempt: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub outcome: Outcome,
    pub cleanup: Cleanup,
    pub acquisition: Acquisition,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Invalid,
    Capacity,
    UnknownRuntime,
    UnknownLease,
    UnknownRecorder,
    UnknownRequest,
    IdentityMismatch,
    Retired,
    Sequence,
    Exhausted,
    CleanupPending,
    NotTerminal,
    NotPending,
    RetirementOrder,
    Poisoned,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retirement {
    Live,
    Retired,
    AckRetired,
    Forgotten,
    Acknowledged,
}
struct Slot {
    request: Request,
    receipt: Receipt,
    retirement: Retirement,
}
struct RuntimeState {
    identity: Runtime,
    lease: String,
    windows: Vec<Window>,
    slots: Vec<Slot>,
}
struct Window {
    recorder_incarnation: u64,
    next: Option<u64>,
}
struct RecorderState {
    identity: Recorder,
    graph: Graph,
    revision: u64,
    active: bool,
    owner: String,
    mapping_generation: u64,
}
struct State {
    runtimes: Vec<RuntimeState>,
    recorders: Vec<RecorderState>,
    last_runtime: Option<u64>,
    last_recorder: Option<u64>,
}
pub(crate) struct Core {
    host: String,
    max_runtimes: usize,
    max_recorders: usize,
    slots_per_recorder: usize,
    state: Mutex<State>,
}

fn valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_FIELD_BYTES
}
fn valid_runtime(r: &Runtime) -> bool {
    valid(&r.host) && valid(&r.session) && valid(&r.runtime)
}
fn valid_recorder(r: &Recorder) -> bool {
    valid(&r.host)
        && valid(&r.peer)
        && valid(&r.group)
        && valid(&r.recording)
        && valid(&r.capability)
}
fn valid_graph(g: &Graph) -> bool {
    valid(&g.owner) && valid(&g.attachment)
}
fn valid_request(r: &Request) -> bool {
    valid_runtime(&r.runtime)
        && valid_recorder(&r.recorder)
        && valid(&r.request_id)
        && valid(&r.lease)
        && valid(&r.expected_owner)
        && valid_graph(&r.expected_graph)
}

impl Core {
    pub(crate) fn new(
        host: String,
        max_runtimes: usize,
        max_recorders: usize,
        slots_per_recorder: usize,
    ) -> Result<Self, Error> {
        if !valid(&host) || max_runtimes == 0 || max_recorders == 0 || slots_per_recorder == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            host,
            max_runtimes,
            max_recorders,
            slots_per_recorder,
            state: Mutex::new(State {
                runtimes: Vec::new(),
                recorders: Vec::new(),
                last_runtime: None,
                last_recorder: None,
            }),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|_| Error::Poisoned)
    }
    pub(crate) fn register_runtime(&self, runtime: Runtime, lease: String) -> Result<(), Error> {
        if !valid_runtime(&runtime) || runtime.host != self.host || !valid(&lease) {
            return Err(Error::Invalid);
        }
        let mut s = self.lock()?;
        if let Some(existing) = s.runtimes.iter().find(|r| r.identity == runtime) {
            return if existing.lease == lease {
                Ok(())
            } else {
                Err(Error::IdentityMismatch)
            };
        }
        if s.last_runtime
            .is_some_and(|last| runtime.incarnation <= last)
        {
            return Err(Error::Retired);
        }
        if s.runtimes.len() == self.max_runtimes {
            return Err(Error::Capacity);
        }
        s.last_runtime = Some(runtime.incarnation);
        s.runtimes.push(RuntimeState {
            identity: runtime,
            lease,
            windows: Vec::new(),
            slots: Vec::new(),
        });
        Ok(())
    }
    pub(crate) fn register_recorder(
        &self,
        recorder: Recorder,
        graph: Graph,
        revision: u64,
        owner: String,
    ) -> Result<(), Error> {
        if !valid_recorder(&recorder)
            || recorder.host != self.host
            || !valid_graph(&graph)
            || !valid(&owner)
        {
            return Err(Error::Invalid);
        }
        if revision == u64::MAX {
            return Err(Error::Exhausted);
        }
        let mut s = self.lock()?;
        // A live peer collection cannot be reset with altered credentials.
        if s.recorders.iter().any(|r| {
            r.identity.host == recorder.host
                && r.identity.peer == recorder.peer
                && r.identity.group == recorder.group
        }) {
            return Err(Error::IdentityMismatch);
        }
        if s.last_recorder
            .is_some_and(|last| recorder.incarnation <= last)
        {
            return Err(Error::Retired);
        }
        if s.recorders.len() == self.max_recorders {
            return Err(Error::Capacity);
        }
        s.last_recorder = Some(recorder.incarnation);
        s.recorders.push(RecorderState {
            identity: recorder,
            graph,
            revision,
            active: true,
            owner,
            mapping_generation: 0,
        });
        Ok(())
    }
    fn locate(s: &State, req: &Request) -> Result<(usize, Option<usize>), Error> {
        if !valid_request(req) || req.runtime.host != req.recorder.host {
            return Err(Error::Invalid);
        }
        let ri = s
            .runtimes
            .iter()
            .position(|r| r.identity == req.runtime)
            .ok_or(Error::UnknownRuntime)?;
        let runtime = &s.runtimes[ri];
        if let Some(si) = runtime.slots.iter().position(|slot| {
            slot.request.sequence == req.sequence
                && slot.request.recorder.incarnation == req.recorder.incarnation
        }) {
            if runtime.slots[si].request != *req {
                return Err(Error::IdentityMismatch);
            }
            if runtime.slots[si].retirement == Retirement::Acknowledged {
                return Err(Error::Retired);
            }
            return Ok((ri, Some(si)));
        }
        if runtime.lease != req.lease {
            return Err(Error::UnknownLease);
        }
        Self::recorder_index(s, &req.recorder)?;
        let next = runtime
            .windows
            .iter()
            .find(|w| w.recorder_incarnation == req.recorder.incarnation)
            .map_or(Some(0), |w| w.next);
        match next {
            Some(next) if req.sequence < next => Err(Error::Retired),
            None => Err(Error::Retired),
            _ => Ok((ri, None)),
        }
    }
    fn recorder_index(s: &State, id: &Recorder) -> Result<usize, Error> {
        s.recorders
            .iter()
            .position(|r| r.identity == *id)
            .ok_or(Error::UnknownRecorder)
    }
    fn rejection(record: &RecorderState, req: &Request) -> Option<Reject> {
        if !record.active || record.revision != req.expected_authority {
            Some(Reject::Authority)
        } else if record.graph != req.expected_graph {
            Some(Reject::Graph)
        } else if record.owner != req.expected_owner {
            Some(Reject::Owner)
        } else if record.mapping_generation != req.expected_mapping {
            Some(Reject::Mapping)
        } else {
            None
        }
    }
    // A bounded window accepts out-of-order dispatch. Acknowledged holes stay
    // as tombstones until every earlier sequence retires, then the high-water
    // mark advances without retaining unbounded per-request history.
    fn admit(
        &self,
        s: &mut State,
        req: &Request,
        outcome: Outcome,
        cleanup: Cleanup,
    ) -> Result<(usize, usize), Error> {
        let (ri, found) = Self::locate(s, req)?;
        if let Some(si) = found {
            return Ok((ri, si));
        }
        Self::recorder_index(s, &req.recorder)?;
        let runtime = &mut s.runtimes[ri];
        if runtime
            .slots
            .iter()
            .filter(|slot| slot.request.recorder.incarnation == req.recorder.incarnation)
            .count()
            == self.slots_per_recorder
        {
            return Err(Error::Capacity);
        }
        let base = runtime
            .windows
            .iter()
            .find(|w| w.recorder_incarnation == req.recorder.incarnation)
            .map_or(Some(0), |w| w.next)
            .ok_or(Error::Exhausted)?;
        let offset = req.sequence.checked_sub(base).ok_or(Error::Retired)?;
        if offset >= u64::try_from(self.slots_per_recorder).map_err(|_| Error::Invalid)? {
            return Err(Error::Sequence);
        }
        if !runtime
            .windows
            .iter()
            .any(|w| w.recorder_incarnation == req.recorder.incarnation)
        {
            runtime.windows.push(Window {
                recorder_incarnation: req.recorder.incarnation,
                next: Some(0),
            });
        }
        runtime.slots.push(Slot {
            request: req.clone(),
            receipt: Receipt {
                outcome,
                cleanup,
                acquisition: Acquisition::NotStarted,
            },
            retirement: Retirement::Live,
        });
        Ok((ri, runtime.slots.len() - 1))
    }
    pub(crate) fn start(&self, req: &Request) -> Result<Receipt, Error> {
        let mut s = self.lock()?;
        let (ri, found) = Self::locate(&s, req)?;
        if let Some(si) = found {
            return Ok(s.runtimes[ri].slots[si].receipt.clone());
        }
        let index = Self::recorder_index(&s, &req.recorder)?;
        let outcome =
            Self::rejection(&s.recorders[index], req).map_or(Outcome::Pending, Outcome::Rejected);
        let (ri, si) = self.admit(&mut s, req, outcome, Cleanup::NotRequired)?;
        Ok(s.runtimes[ri].slots[si].receipt.clone())
    }
    // Register before dispatch; an acknowledgement retry never dispatches again.
    pub(crate) fn begin_acquisition(
        &self,
        req: &Request,
        attempt: &str,
    ) -> Result<AcquisitionDispatch, Error> {
        if !valid(attempt) {
            return Err(Error::Invalid);
        }
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        match &slot.receipt.acquisition {
            Acquisition::NotStarted if slot.receipt.outcome == Outcome::Pending => {
                slot.receipt.acquisition = Acquisition::Acquiring {
                    attempt: attempt.into(),
                };
                slot.receipt.cleanup = Cleanup::Pending;
                Ok(AcquisitionDispatch::Dispatch)
            }
            Acquisition::Acquiring { attempt: existing } if existing == attempt => {
                Ok(AcquisitionDispatch::AlreadyRegistered)
            }
            Acquisition::Ready {
                attempt: existing, ..
            }
            | Acquisition::Failed { attempt: existing }
                if existing == attempt =>
            {
                Ok(AcquisitionDispatch::AlreadyRegistered)
            }
            Acquisition::NotStarted => Err(Error::NotPending),
            _ => Err(Error::IdentityMismatch),
        }
    }
    pub(crate) fn acquisition_ready(
        &self,
        req: &Request,
        attempt: &str,
        permit: &str,
    ) -> Result<(), Error> {
        if !valid(attempt) || !valid(permit) {
            return Err(Error::Invalid);
        }
        let mut s = self.lock()?;
        let (ri, si) = Self::locate(&s, req)?;
        let si = si.ok_or(Error::UnknownRequest)?;
        if matches!(&s.runtimes[ri].slots[si].receipt.acquisition,
            Acquisition::Ready { attempt: existing, permit: held } if existing == attempt && held == permit)
        {
            return Ok(());
        }
        // A permit is an exclusive resource, not a reusable readiness flag.
        // A conflicting native completion is a protocol fault: its caller must
        // supervise that result externally while this attempt remains uncertain.
        if s.runtimes.iter().enumerate().any(|(r, runtime)| runtime.slots.iter().enumerate().any(|(i, slot)| {
            (r != ri || i != si) && slot.receipt.cleanup != Cleanup::Released &&
                matches!(&slot.receipt.acquisition, Acquisition::Ready { permit: held, .. } if held == permit)
        })) { return Err(Error::IdentityMismatch); }
        let slot = &mut s.runtimes[ri].slots[si];
        match &slot.receipt.acquisition {
            Acquisition::Acquiring { attempt: existing } if existing == attempt => {
                slot.receipt.acquisition = Acquisition::Ready {
                    attempt: attempt.into(),
                    permit: permit.into(),
                };
                Ok(())
            }
            Acquisition::Ready {
                attempt: existing,
                permit: held,
            } if existing == attempt && held == permit => Ok(()),
            _ => Err(Error::IdentityMismatch),
        }
    }
    pub(crate) fn acquisition_failed(&self, req: &Request, attempt: &str) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        match &slot.receipt.acquisition {
            Acquisition::Acquiring { attempt: existing } if existing == attempt => {
                slot.receipt.acquisition = Acquisition::Failed {
                    attempt: attempt.into(),
                };
                slot.receipt.cleanup = Cleanup::NotRequired;
                Ok(())
            }
            Acquisition::Failed { attempt: existing } if existing == attempt => Ok(()),
            _ => Err(Error::IdentityMismatch),
        }
    }
    pub(crate) fn status(&self, req: &Request) -> Result<Receipt, Error> {
        let s = self.lock()?;
        let (ri, si) = Self::locate(&s, req)?;
        Ok(s.runtimes[ri].slots[si.ok_or(Error::UnknownRequest)?]
            .receipt
            .clone())
    }
    pub(crate) fn fence(&self, req: &Request) -> Result<Receipt, Error> {
        let mut s = self.lock()?;
        let (ri, si) = self.admit(&mut s, req, Outcome::FencedNoEffect, Cleanup::NotRequired)?;
        let receipt = &mut s.runtimes[ri].slots[si].receipt;
        if receipt.outcome == Outcome::Pending {
            receipt.outcome = Outcome::FencedNoEffect;
        }
        Ok(receipt.clone())
    }
    pub(crate) fn commit(&self, req: &Request, permit: &str) -> Result<Receipt, Error> {
        let mut s = self.lock()?;
        let (ri, si) = Self::locate(&s, req)?;
        let si = si.ok_or(Error::UnknownRequest)?;
        if s.runtimes[ri].slots[si].receipt.outcome != Outcome::Pending {
            return Ok(s.runtimes[ri].slots[si].receipt.clone());
        }
        match &s.runtimes[ri].slots[si].receipt.acquisition {
            Acquisition::Ready { permit: held, .. } if held == permit => {}
            Acquisition::Ready { .. } => return Err(Error::IdentityMismatch),
            _ => return Err(Error::NotPending),
        }
        let index = Self::recorder_index(&s, &req.recorder)?;
        let record = &mut s.recorders[index];
        let outcome = if let Some(reject) = Self::rejection(record, req) {
            Outcome::Rejected(reject)
        } else {
            let next = record
                .mapping_generation
                .checked_add(1)
                .ok_or(Error::Exhausted)?;
            record.owner = req.lease.clone();
            record.mapping_generation = next;
            Outcome::Committed {
                graph: record.graph.clone(),
                authority: record.revision,
            }
        };
        let receipt = &mut s.runtimes[ri].slots[si].receipt;
        receipt.outcome = outcome;
        Ok(receipt.clone())
    }
    pub(crate) fn mapping(&self, recorder: &Recorder) -> Result<String, Error> {
        let s = self.lock()?;
        Ok(s.recorders[Self::recorder_index(&s, recorder)?]
            .owner
            .clone())
    }
    pub(crate) fn mapping_generation(&self, recorder: &Recorder) -> Result<u64, Error> {
        let s = self.lock()?;
        Ok(s.recorders[Self::recorder_index(&s, recorder)?].mapping_generation)
    }
    pub(crate) fn update_graph(&self, recorder: &Recorder, graph: Graph) -> Result<(), Error> {
        if !valid_graph(&graph) {
            return Err(Error::Invalid);
        }
        let mut s = self.lock()?;
        let i = Self::recorder_index(&s, recorder)?;
        if graph == s.recorders[i].graph {
            return Ok(());
        }
        if graph.revision <= s.recorders[i].graph.revision {
            return Err(Error::Sequence);
        }
        s.recorders[i].graph = graph;
        Ok(())
    }
    pub(crate) fn advance_authority(&self, recorder: &Recorder) -> Result<(), Error> {
        let mut s = self.lock()?;
        let i = Self::recorder_index(&s, recorder)?;
        let next = s.recorders[i]
            .revision
            .checked_add(1)
            .ok_or(Error::Exhausted)?;
        // Reserve the final revision for terminal revocation.
        if next == u64::MAX {
            return Err(Error::Exhausted);
        }
        s.recorders[i].revision = next;
        Ok(())
    }
    pub(crate) fn stop(&self, recorder: &Recorder) -> Result<(), Error> {
        let mut s = self.lock()?;
        let i = Self::recorder_index(&s, recorder)?;
        if !s.recorders[i].active {
            return Ok(());
        }
        let revision = s.recorders[i]
            .revision
            .checked_add(1)
            .ok_or(Error::Exhausted)?;
        s.recorders[i].revision = revision;
        s.recorders[i].active = false;
        for runtime in &mut s.runtimes {
            for slot in &mut runtime.slots {
                if slot.request.recorder == *recorder && slot.receipt.outcome == Outcome::Pending {
                    slot.receipt.outcome = Outcome::FencedNoEffect;
                }
            }
        }
        Ok(())
    }
    fn slot_mut<'a>(s: &'a mut State, req: &Request) -> Result<&'a mut Slot, Error> {
        let (ri, si) = Self::locate(s, req)?;
        Ok(&mut s.runtimes[ri].slots[si.ok_or(Error::UnknownRequest)?])
    }
    pub(crate) fn cleanup_failed(&self, req: &Request, permit: &str) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        if slot.receipt.outcome == Outcome::Pending {
            return Err(Error::NotTerminal);
        }
        if !matches!(&slot.receipt.acquisition, Acquisition::Ready { permit: held, .. } if held == permit)
        {
            return Err(Error::IdentityMismatch);
        }
        if matches!(slot.receipt.cleanup, Cleanup::Pending | Cleanup::Failed) {
            slot.receipt.cleanup = Cleanup::Failed;
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    pub(crate) fn cleanup_released(&self, req: &Request, permit: &str) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        if slot.receipt.outcome == Outcome::Pending {
            return Err(Error::NotTerminal);
        }
        if !matches!(&slot.receipt.acquisition, Acquisition::Ready { permit: held, .. } if held == permit)
        {
            return Err(Error::IdentityMismatch);
        }
        if matches!(
            slot.receipt.cleanup,
            Cleanup::Pending | Cleanup::Failed | Cleanup::Released
        ) {
            slot.receipt.cleanup = Cleanup::Released;
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    pub(crate) fn retire(&self, req: &Request) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        if slot.receipt.outcome == Outcome::Pending {
            return Err(Error::NotTerminal);
        }
        if matches!(slot.receipt.cleanup, Cleanup::Pending | Cleanup::Failed) {
            return Err(Error::CleanupPending);
        }
        if slot.retirement == Retirement::Live {
            slot.retirement = Retirement::Retired;
        }
        Ok(())
    }
    pub(crate) fn ack_retire(&self, req: &Request) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        match slot.retirement {
            Retirement::Live => Err(Error::RetirementOrder),
            Retirement::Retired => {
                slot.retirement = Retirement::AckRetired;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn forget_retire(&self, req: &Request) -> Result<(), Error> {
        let mut s = self.lock()?;
        let slot = Self::slot_mut(&mut s, req)?;
        match slot.retirement {
            Retirement::AckRetired => {
                slot.retirement = Retirement::Forgotten;
                Ok(())
            }
            Retirement::Forgotten => Ok(()),
            _ => Err(Error::RetirementOrder),
        }
    }
    pub(crate) fn ack_forget(&self, req: &Request) -> Result<(), Error> {
        let mut s = self.lock()?;
        // Acknowledged out-of-order tombstones retain exact identity and can
        // replay this acknowledgement until compaction removes them.
        let ri = s
            .runtimes
            .iter()
            .position(|r| r.identity == req.runtime)
            .ok_or(Error::UnknownRuntime)?;
        if let Some(slot) = s.runtimes[ri].slots.iter().find(|slot| {
            slot.request.sequence == req.sequence
                && slot.request.recorder.incarnation == req.recorder.incarnation
        }) {
            if slot.request != *req {
                return Err(Error::IdentityMismatch);
            }
            if slot.retirement == Retirement::Acknowledged {
                return Ok(());
            }
        }
        let (ri, si) = Self::locate(&s, req)?;
        let si = si.ok_or(Error::UnknownRequest)?;
        if s.runtimes[ri].slots[si].retirement != Retirement::Forgotten {
            return Err(Error::RetirementOrder);
        }
        let runtime = &mut s.runtimes[ri];
        runtime.slots[si].retirement = Retirement::Acknowledged;
        let wi = runtime
            .windows
            .iter()
            .position(|w| w.recorder_incarnation == req.recorder.incarnation)
            .ok_or(Error::UnknownRecorder)?;
        while let Some(base) = runtime.windows[wi].next {
            let Some(index) = runtime.slots.iter().position(|slot| {
                slot.request.recorder.incarnation == req.recorder.incarnation
                    && slot.request.sequence == base
                    && slot.retirement == Retirement::Acknowledged
            }) else {
                break;
            };
            runtime.slots.swap_remove(index);
            runtime.windows[wi].next = base.checked_add(1);
        }
        Ok(())
    }
    // Caller authenticates teardown and guarantees the old incarnation cannot
    // issue further work. Unacknowledged receipts and cleanup debt survive.
    pub(crate) fn retire_runtime(&self, runtime: &Runtime, lease: &str) -> Result<(), Error> {
        let mut s = self.lock()?;
        let i = s
            .runtimes
            .iter()
            .position(|r| r.identity == *runtime)
            .ok_or(Error::UnknownRuntime)?;
        if s.runtimes[i].lease != lease {
            return Err(Error::UnknownLease);
        }
        if s.runtimes[i]
            .slots
            .iter()
            .any(|slot| slot.retirement != Retirement::Acknowledged)
        {
            return Err(Error::CleanupPending);
        }
        s.runtimes.swap_remove(i);
        Ok(())
    }
    pub(crate) fn retire_recorder(&self, recorder: &Recorder) -> Result<(), Error> {
        let mut s = self.lock()?;
        let i = Self::recorder_index(&s, recorder)?;
        if s.recorders[i].active {
            return Err(Error::NotTerminal);
        }
        if s.runtimes.iter().any(|runtime| {
            runtime.slots.iter().any(|slot| {
                slot.request.recorder == *recorder && slot.retirement != Retirement::Acknowledged
            })
        }) {
            return Err(Error::CleanupPending);
        }
        for runtime in &mut s.runtimes {
            runtime
                .slots
                .retain(|slot| slot.request.recorder != *recorder);
            runtime
                .windows
                .retain(|window| window.recorder_incarnation != recorder.incarnation);
        }
        s.recorders.swap_remove(i);
        Ok(())
    }
}
