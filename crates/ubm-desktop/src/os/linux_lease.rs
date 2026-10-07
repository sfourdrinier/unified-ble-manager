//! Owned Linux lease operations. Tests run the same task/ledger on every host;
//! they are not daemon or physical-radio qualification.

use crate::errors::{DesktopError, PlatformDetail, PlatformValue};
use futures_util::{FutureExt, future::Shared};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, oneshot};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Physical,
    Reservation,
    Protected,
    Indeterminate,
}
impl Scope {
    fn native_name(self) -> &'static str {
        match self {
            Self::Physical => "physical-released",
            Self::Reservation => "reservation-released",
            Self::Protected => "lease-released-protected",
            Self::Indeterminate => "lease-released-indeterminate",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Receipt {
    pub token: u64,
    pub generation: u64,
    pub scope: Scope,
    pub disconnect_reason: Option<u8>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReleaseObservation {
    pub physical_generation: Option<u64>,
    pub disconnect_reason: Option<u8>,
}

pub(crate) trait LeaseClient: Clone + Send + Sync + 'static {
    /// Only a bus-confirmed vanished unique daemon owner retires its obligations.
    fn owner_retired(&self) -> impl Future<Output = Result<bool, DesktopError>> + Send;
    fn allocate_reservation(&self) -> Result<u64, DesktopError>;
    fn reserve(&self, reservation: u64) -> impl Future<Output = Result<u64, DesktopError>> + Send;
    fn recover(
        &self,
        reservation: u64,
    ) -> impl Future<Output = Result<Option<u64>, DesktopError>> + Send;
    fn connect(&self, token: u64) -> impl Future<Output = Result<u64, DesktopError>> + Send;
    fn release(
        &self,
        token: u64,
        generation: Option<u64>,
    ) -> impl Future<Output = Result<Receipt, DesktopError>> + Send;
    fn acknowledge(&self, token: u64) -> impl Future<Output = Result<(), DesktopError>> + Send;
    fn replay_loss(
        &self,
        generation: u64,
        reason: u8,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send;
}
#[derive(Clone, Copy)]
struct Token {
    id: u64,
    generation: Option<u64>,
}
enum State {
    UnresolvedReservation,
    Owned(Token),
    Released(ReleaseObservation),
}
struct Entry<C> {
    client: C,
    reservation: u64,
    token: Arc<Mutex<State>>,
    physical_generation: AtomicU64,
    loss_reported: AtomicBool,
    pending_losses: StdMutex<BTreeMap<u64, u8>>,
    observation_failure: AtomicBool,
}
pub(crate) struct Ledger<C> {
    entries: Arc<StdMutex<HashMap<String, Arc<Entry<C>>>>>,
    terminal_facts: Arc<StdMutex<HashMap<String, Arc<Entry<C>>>>>,
    maintenance: Arc<StdMutex<HashMap<u64, Arc<Acknowledgment<C>>>>>,
}
struct Acknowledgment<C> {
    client: C,
    token: u64,
    done: Mutex<bool>,
    attempt: StdMutex<Option<AcknowledgmentAttempt>>,
}
type AcknowledgmentAttempt = Shared<Pin<Box<dyn Future<Output = Result<(), DesktopError>> + Send>>>;
impl<C> Clone for Ledger<C> {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
            terminal_facts: Arc::clone(&self.terminal_facts),
            maintenance: Arc::clone(&self.maintenance),
        }
    }
}
impl<C> Default for Ledger<C> {
    fn default() -> Self {
        Self {
            entries: Arc::default(),
            terminal_facts: Arc::default(),
            maintenance: Arc::default(),
        }
    }
}
fn failed(detail: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Cleanup,
        "connection.disconnect",
    )
    .with_detail(detail)
}
struct Cancel<C: LeaseClient> {
    ledger: Ledger<C>,
    peer: String,
    entry: Arc<Entry<C>>,
    committed: bool,
    executor: tokio::runtime::Handle,
}
impl<C: LeaseClient> Drop for Cancel<C> {
    fn drop(&mut self) {
        if !self.committed {
            let ledger = self.ledger.clone();
            let peer = self.peer.clone();
            let entry = Arc::clone(&self.entry);
            self.executor.spawn(async move {
                if let Err(error) = ledger.release_entry(&peer, &entry).await {
                    // The ledger retains this exact token for retry; logging does not retire it.
                    eprintln!(
                        "ubm-desktop: cancelled lease acquisition retains cleanup debt: {error:?}"
                    );
                }
            });
        }
    }
}
impl<C: LeaseClient> Ledger<C> {
    pub(crate) fn peers(&self) -> Vec<String> {
        self.entries
            .lock()
            .expect("lease ledger")
            .keys()
            .cloned()
            .collect()
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
    fn retire(&self, peer: &str, entry: &Arc<Entry<C>>) {
        let mut entries = self.entries.lock().expect("lease ledger");
        if entries
            .get(peer)
            .is_some_and(|owned| Arc::ptr_eq(owned, entry))
        {
            entries.remove(peer);
            if entry.physical_generation.load(Ordering::Acquire) != 0 {
                let mut facts = self.terminal_facts.lock().expect("terminal release facts");
                if !entry.loss_reported.load(Ordering::Acquire) {
                    facts.insert(peer.to_owned(), Arc::clone(entry));
                }
            }
        }
    }
    #[cfg(test)]
    fn terminal_facts_len(&self) -> usize {
        self.terminal_facts.lock().unwrap().len()
    }
    pub(crate) fn consume_terminal(&self, peer: &str, generation: u64) {
        let mut facts = self.terminal_facts.lock().expect("terminal release facts");
        if facts
            .get(peer)
            .is_some_and(|entry| entry.physical_generation.load(Ordering::Acquire) == generation)
        {
            facts.remove(peer);
        }
    }
    pub(crate) fn with_release_scope<T>(
        &self,
        peer: &str,
        generation: Option<u64>,
        cleanup: impl FnOnce() -> T,
    ) -> Option<T> {
        // Admission and retained facts share this lock order. A newer provisional
        // entry already supersedes old cleanup, even before its Connect answers.
        let entries = self.entries.lock().expect("lease ledger");
        if entries.contains_key(peer) {
            return None;
        }
        let facts = self.terminal_facts.lock().expect("terminal release facts");
        if facts.get(peer).is_some_and(|entry| {
            !generation.is_some_and(|generation| {
                entry.physical_generation.load(Ordering::Acquire) == generation
            })
        }) {
            return None;
        }
        drop(facts);
        // Keep admission excluded until synchronous local cleanup finishes.
        Some(cleanup())
    }
    /// Install provisional ownership before starting the native request. Dropping
    /// the waiter never drops the request: its retained task settles the token,
    /// and the cancellation guard queues exact-owner compensation behind it.
    pub(crate) async fn connect(self, peer: String, client: C) -> Result<(), DesktopError> {
        self.schedule_maintenance();
        self.connect_owned(peer, client).await
    }
    async fn connect_owned(self, peer: String, client: C) -> Result<(), DesktopError> {
        let (entry, mut state) = {
            let mut entries = self.entries.lock().expect("lease ledger");
            if entries.contains_key(&peer) {
                return Err(failed("an earlier lease for this peer remains owned"));
            }
            // Allocate only after peer admission. Every allocated sender nonce
            // is then owned before any await and recoverable even if Reserve
            // never reaches the daemon; refused duplicate work leaves no gaps.
            let reservation = client.allocate_reservation()?;
            if reservation == 0 {
                return Err(failed("private reservation identity must be nonzero"));
            }
            let token = Arc::new(Mutex::new(State::UnresolvedReservation));
            let state = Arc::clone(&token)
                .try_lock_owned()
                .expect("new unpublished lease state");
            let entry = Arc::new(Entry {
                client,
                reservation,
                token,
                physical_generation: AtomicU64::new(0),
                loss_reported: AtomicBool::new(false),
                pending_losses: StdMutex::new(BTreeMap::new()),
                observation_failure: AtomicBool::new(false),
            });
            entries.insert(peer.clone(), Arc::clone(&entry));
            self.terminal_facts
                .lock()
                .expect("terminal release facts")
                .remove(&peer);
            (entry, state)
        };
        // Reserve/connect worker takes the lock across both accepted futures.
        // Pass an owned mutex guard instead of racing publication with spawning.
        let worker_entry = Arc::clone(&entry);
        let (tx, rx) = oneshot::channel();
        let mut cancel = Cancel {
            ledger: self.clone(),
            peer: peer.clone(),
            entry: Arc::clone(&entry),
            committed: false,
            executor: tokio::runtime::Handle::current(),
        };
        tokio::spawn(async move {
            let outcome = async {
                let token = worker_entry
                    .client
                    .reserve(worker_entry.reservation)
                    .await?;
                if token == 0 {
                    return Err(failed("daemon returned a zero lease token"));
                }
                *state = State::Owned(Token {
                    id: token,
                    generation: None,
                });
                if tx.is_closed() {
                    return Err(failed("acquisition waiter was cancelled after reservation"));
                }
                let generation = worker_entry.client.connect(token).await?;
                if generation == 0 {
                    return Err(failed("daemon returned a zero physical LE generation"));
                }
                *state = State::Owned(Token {
                    id: token,
                    generation: Some(generation),
                });
                worker_entry
                    .physical_generation
                    .store(generation, Ordering::Release);
                if worker_entry.observation_failure.load(Ordering::Acquire) {
                    return Err(DesktopError::new(BleErrorCode::PlatformFailure, BleErrorDomain::Connection,
                        "connection.connect").with_detail("authenticated early physical-loss observation overflow; acquisition refused"));
                }
                let observed = worker_entry.pending_losses.lock().expect("pending physical loss").remove(&generation);
                if let Some(reason) = observed { worker_entry.client.replay_loss(generation, reason).await?; }
                Ok(())
            }
            .await;
            let _ = tx.send(outcome);
        });
        let result = rx
            .await
            .map_err(|_| failed("retained lease acquisition task failed"))?;
        if result.is_ok() {
            cancel.committed = true;
        }
        result
    }
    async fn release_entry(
        &self,
        peer: &str,
        entry: &Arc<Entry<C>>,
    ) -> Result<ReleaseObservation, DesktopError> {
        let mut state = entry.token.lock().await;
        // Confirmed native release is a retained fact, not a new bus query.
        // Acknowledgment maintenance remains independently owned below.
        if let State::Released(observation) = *state {
            return Ok(observation);
        }
        if entry.client.owner_retired().await? {
            return Ok(self.retire_owner(peer, entry, &mut state));
        }
        if matches!(*state, State::UnresolvedReservation) {
            // Read-only exact-ID reconciliation installs a daemon no-admission
            // fence on None. It never allocates a replacement reservation.
            let recovered = match entry.client.recover(entry.reservation).await {
                Ok(value) => value,
                Err(error) => {
                    if entry.client.owner_retired().await? {
                        return Ok(self.retire_owner(peer, entry, &mut state));
                    }
                    return Err(error);
                }
            };
            match recovered {
                Some(token) if token != 0 => {
                    *state = State::Owned(Token {
                        id: token,
                        generation: None,
                    })
                }
                Some(_) => return Err(failed("recovery returned a zero owned token")),
                None => {
                    *state = State::Released(ReleaseObservation::default());
                    self.retire(peer, entry);
                    return Ok(ReleaseObservation::default());
                }
            }
        }
        let token = match *state {
            State::Owned(token) => token,
            State::Released(reason) => return Ok(reason),
            State::UnresolvedReservation => {
                return Err(failed(
                    "reservation admission is indeterminate; exact native reservation reconciliation is required",
                ));
            }
        };
        let reason = {
            let receipt = match entry.client.release(token.id, token.generation).await {
                Ok(value) => value,
                Err(error) => {
                    if entry.client.owner_retired().await? {
                        return Ok(self.retire_owner(peer, entry, &mut state));
                    }
                    return Err(error);
                }
            };
            if receipt.token != token.id
                || token
                    .generation
                    .is_some_and(|generation| receipt.generation != generation)
            {
                return Err(failed(
                    "release receipt does not match the owned lease identity",
                ));
            }
            match receipt.scope {
                Scope::Physical if receipt.generation != 0 => {}
                Scope::Reservation if receipt.generation == 0 && token.generation.is_none() => {}
                // The daemon retains a generation-scoped cleanup obligation
                // when this logical token is ACKed. This reports no ACL end.
                Scope::Protected
                    if receipt.generation != 0
                        && token.generation == Some(receipt.generation)
                        && receipt.disconnect_reason.is_none() =>
                {}
                _ => return Err(failed("daemon did not confirm physical or uneffected reservation release; lease remains owned")
                    .with_platform(PlatformDetail::new("bluez-le-lease", receipt.scope.native_name())
                        .with_metadata("token", PlatformValue::Text(receipt.token.to_string()))
                        .with_metadata("physicalGeneration", PlatformValue::Text(receipt.generation.to_string())))),
            }
            if receipt.disconnect_reason.is_some() && receipt.scope != Scope::Physical {
                return Err(failed(
                    "nonphysical release receipt carries a physical disconnect reason",
                ));
            }
            ReleaseObservation {
                physical_generation: (receipt.scope == Scope::Physical)
                    .then_some(receipt.generation),
                disconnect_reason: receipt.disconnect_reason,
            }
        };
        *state = State::Released(reason);
        let acknowledgment = Arc::new(Acknowledgment {
            client: entry.client.clone(),
            token: token.id,
            done: Mutex::new(false),
            attempt: StdMutex::new(None),
        });
        self.maintenance
            .lock()
            .expect("lease maintenance")
            .insert(entry.reservation, Arc::clone(&acknowledgment));
        if reason.physical_generation.is_none() {
            // A protected logical release transfers reconciliation to the
            // daemon; it is not a retained observation of physical loss.
            entry.physical_generation.store(0, Ordering::Release);
        }
        self.retire(peer, entry);
        let reservation = entry.reservation;
        drop(self.start_acknowledgment(reservation, &acknowledgment));
        Ok(reason)
    }
    fn retire_owner(
        &self,
        peer: &str,
        entry: &Arc<Entry<C>>,
        state: &mut State,
    ) -> ReleaseObservation {
        // This retires daemon-owned tokens only. It reports no invented ACL
        // generation/reason, and does not touch local streams, matches or handlers.
        let observation = match *state {
            State::Released(observation) => observation,
            _ => ReleaseObservation::default(),
        };
        *state = State::Released(observation);
        entry.loss_reported.store(true, Ordering::Release);
        self.consume_terminal(peer, entry.physical_generation.load(Ordering::Acquire));
        self.retire(peer, entry);
        observation
    }
    fn start_acknowledgment(
        &self,
        reservation: u64,
        acknowledgment: &Arc<Acknowledgment<C>>,
    ) -> AcknowledgmentAttempt {
        let mut attempt = acknowledgment
            .attempt
            .lock()
            .expect("lease acknowledgment attempt");
        if let Some(current) = &*attempt
            && !matches!(current.peek(), Some(Err(_)))
        {
            return current.clone();
        }
        let ledger = self.clone();
        let owned = Arc::clone(acknowledgment);
        let retained = async move { ledger.acknowledge(reservation, &owned).await }
            .boxed()
            .shared();
        *attempt = Some(retained.clone());
        let driver = retained.clone();
        // Exactly one independent driver polls this attempt. Bounded observers
        // clone its completion, never enqueue more held native mutex waiters.
        tokio::spawn(async move {
            if let Err(error) = driver.await {
                eprintln!(
                    "ubm-desktop: released lease retains acknowledgment maintenance debt: {error:?}"
                );
            }
        });
        retained
    }
    fn schedule_maintenance(&self) {
        let owned: Vec<_> = self
            .maintenance
            .lock()
            .expect("lease maintenance")
            .iter()
            .map(|(id, task)| (*id, Arc::clone(task)))
            .collect();
        for (id, task) in owned {
            drop(self.start_acknowledgment(id, &task));
        }
    }
    async fn acknowledge(
        &self,
        reservation: u64,
        acknowledgment: &Arc<Acknowledgment<C>>,
    ) -> Result<(), DesktopError> {
        let mut done = acknowledgment.done.lock().await;
        if !*done {
            if !acknowledgment.client.owner_retired().await?
                && let Err(error) = acknowledgment
                    .client
                    .acknowledge(acknowledgment.token)
                    .await
                && !acknowledgment.client.owner_retired().await?
            {
                return Err(error);
            }
            *done = true;
            let mut maintenance = self.maintenance.lock().expect("lease maintenance");
            if maintenance
                .get(&reservation)
                .is_some_and(|owned| Arc::ptr_eq(owned, acknowledgment))
            {
                maintenance.remove(&reservation);
            }
        }
        Ok(())
    }
    pub(crate) async fn retry_maintenance(&self) -> Vec<DesktopError> {
        let owned: Vec<_> = self
            .maintenance
            .lock()
            .expect("lease maintenance")
            .iter()
            .map(|(id, task)| (*id, Arc::clone(task)))
            .collect();
        let mut failures = Vec::new();
        for (id, task) in owned {
            match self.start_acknowledgment(id, &task).await {
                Ok(()) => {}
                Err(error) => failures.push(error),
            }
        }
        failures
    }
    #[cfg(test)]
    fn maintenance_len(&self) -> usize {
        self.maintenance.lock().unwrap().len()
    }
    #[cfg(test)]
    fn maintenance_owners(&self) -> usize {
        self.maintenance
            .lock()
            .unwrap()
            .values()
            .map(Arc::strong_count)
            .sum()
    }
    pub(crate) async fn release(self, peer: &str) -> Result<(), DesktopError> {
        self.release_with_observation(peer).await.map(|_| ())
    }
    pub(crate) async fn release_with_observation(
        self,
        peer: &str,
    ) -> Result<ReleaseObservation, DesktopError> {
        let entry = self
            .entries
            .lock()
            .expect("lease ledger")
            .get(peer)
            .cloned()
            .or_else(|| {
                self.terminal_facts
                    .lock()
                    .expect("terminal release facts")
                    .get(peer)
                    .cloned()
            });
        let Some(entry) = entry else {
            return Ok(ReleaseObservation::default());
        };
        let ledger = self.clone();
        let peer = peer.to_owned();
        let task = tokio::spawn(async move {
            let result = ledger.release_entry(&peer, &entry).await;
            if let Err(error) = &result {
                eprintln!("ubm-desktop: lease release remains owned for {peer}: {error:?}");
            }
            result
        });
        task.await
            .map_err(|_| failed("retained lease release task failed"))?
    }

    /// An authenticated daemon loss generation is evidence for this lease,
    /// never for a newer physical link at the same device path. The exact
    /// entry captured here survives event sinks and release-waiter cancellation.
    #[cfg(test)]
    async fn physical_lost(&self, peer: &str, generation: u64) -> bool {
        self.physical_lost_observed(peer, generation, 2).await
    }
    pub(crate) async fn physical_lost_observed(
        &self,
        peer: &str,
        generation: u64,
        reason: u8,
    ) -> bool {
        let entry = self
            .entries
            .lock()
            .expect("lease ledger")
            .get(peer)
            .cloned()
            .or_else(|| {
                self.terminal_facts
                    .lock()
                    .expect("terminal release facts")
                    .get(peer)
                    .cloned()
            });
        let Some(entry) = entry else {
            return false;
        };
        if generation != 0 && entry.physical_generation.load(Ordering::Acquire) == 0 {
            let mut pending = entry.pending_losses.lock().expect("pending physical loss");
            if entry.physical_generation.load(Ordering::Acquire) == 0 {
                if pending.contains_key(&generation) {
                    return false;
                }
                if pending.len() == 64 {
                    entry.observation_failure.store(true, Ordering::Release);
                    eprintln!(
                        "ubm-desktop: authenticated early physical-loss intake overflow; acquisition will fail closed"
                    );
                } else {
                    pending.insert(generation, reason);
                }
                return false;
            }
        }
        // Event admission never waits on native cleanup's serialization lock.
        // This immutable generation is published once by Connect's own answer.
        let matches = generation != 0
            && entry.physical_generation.load(Ordering::Acquire) == generation
            && entry
                .loss_reported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        if !matches {
            return false;
        }
        self.consume_terminal(peer, generation);
        let ledger = self.clone();
        let peer = peer.to_owned();
        tokio::spawn(async move {
            if let Err(error) = ledger.release_entry(&peer, &entry).await {
                eprintln!(
                    "ubm-desktop: physical-loss lease cleanup remains owned for {peer}: {error:?}"
                );
            }
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::Semaphore;
    type Shared<T> = Arc<StdMutex<T>>;

    #[derive(Clone)]
    struct Client {
        token: u64,
        generation: u64,
        reserve: Arc<Semaphore>,
        connect: Arc<Semaphore>,
        calls: Shared<Vec<(u64, Option<u64>)>>,
        receipts: Shared<VecDeque<Result<Receipt, DesktopError>>>,
        connect_failure: Shared<Option<DesktopError>>,
        reserve_failure: Shared<Option<DesktopError>>,
        recovery: Shared<VecDeque<Result<Option<u64>, DesktopError>>>,
        reservation_calls: Shared<Vec<(bool, u64)>>,
        acknowledgments: Shared<Vec<u64>>,
        acknowledgment_failures: Shared<VecDeque<DesktopError>>,
        release_gate: Shared<Option<Arc<Semaphore>>>,
        replayed_losses: Shared<Vec<(u64, u8)>>,
        replay_failure: Shared<Option<DesktopError>>,
        acknowledgment_gate: Shared<Option<Arc<Semaphore>>>,
        allocations: Arc<AtomicU64>,
        owner_retired: Arc<AtomicBool>,
        owner_query_failed: Arc<AtomicBool>,
        owner_queries: Arc<AtomicU64>,
    }
    impl Client {
        fn new() -> Self {
            Self {
                token: 41,
                owner_retired: Arc::new(AtomicBool::new(false)),
                owner_query_failed: Arc::new(AtomicBool::new(false)),
                owner_queries: Arc::new(AtomicU64::new(0)),
                generation: 73,
                reserve: Arc::new(Semaphore::new(0)),
                connect: Arc::new(Semaphore::new(0)),
                calls: Arc::default(),
                receipts: Arc::default(),
                connect_failure: Arc::default(),
                reserve_failure: Arc::default(),
                recovery: Arc::default(),
                reservation_calls: Arc::default(),
                acknowledgments: Arc::default(),
                acknowledgment_failures: Arc::default(),
                release_gate: Arc::default(),
                replayed_losses: Arc::default(),
                replay_failure: Arc::default(),
                acknowledgment_gate: Arc::default(),
                allocations: Arc::default(),
            }
        }
    }
    impl LeaseClient for Client {
        async fn owner_retired(&self) -> Result<bool, DesktopError> {
            self.owner_queries.fetch_add(1, Ordering::Relaxed);
            if self.owner_query_failed.load(Ordering::Acquire) {
                return Err(failed("bus owner query refused"));
            }
            Ok(self.owner_retired.load(Ordering::Acquire))
        }
        async fn replay_loss(&self, generation: u64, reason: u8) -> Result<(), DesktopError> {
            self.replayed_losses
                .lock()
                .unwrap()
                .push((generation, reason));
            if let Some(error) = self.replay_failure.lock().unwrap().take() {
                Err(error)
            } else {
                Ok(())
            }
        }
        fn allocate_reservation(&self) -> Result<u64, DesktopError> {
            self.allocations.fetch_add(1, Ordering::Relaxed);
            static TEST_SENDER_SERIAL: AtomicU64 = AtomicU64::new(1);
            TEST_SENDER_SERIAL
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| failed("test sender identities exhausted"))
        }
        async fn acknowledge(&self, token: u64) -> Result<(), DesktopError> {
            self.acknowledgments.lock().unwrap().push(token);
            let gate = self.acknowledgment_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            if let Some(error) = self.acknowledgment_failures.lock().unwrap().pop_front() {
                Err(error)
            } else {
                Ok(())
            }
        }
        async fn reserve(&self, reservation: u64) -> Result<u64, DesktopError> {
            self.reservation_calls
                .lock()
                .unwrap()
                .push((false, reservation));
            self.reserve.acquire().await.unwrap().forget();
            if let Some(error) = self.reserve_failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self.token)
        }
        async fn recover(&self, reservation: u64) -> Result<Option<u64>, DesktopError> {
            self.reservation_calls
                .lock()
                .unwrap()
                .push((true, reservation));
            self.recovery
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(failed("recovery unavailable")))
        }
        async fn connect(&self, _: u64) -> Result<u64, DesktopError> {
            self.connect.acquire().await.unwrap().forget();
            if let Some(error) = self.connect_failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self.generation) // Physical LE generation, never ATT attachment identity.
        }
        async fn release(
            &self,
            token: u64,
            generation: Option<u64>,
        ) -> Result<Receipt, DesktopError> {
            let gate = self.release_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            self.calls.lock().unwrap().push((token, generation));
            self.receipts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Receipt {
                    token,
                    generation: generation.unwrap_or(0),
                    scope: if generation.is_some() {
                        Scope::Physical
                    } else {
                        Scope::Reservation
                    },
                    disconnect_reason: None,
                }))
        }
    }
    async fn settled() {
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn vanished_owner_retires_only_its_owned_lease_without_release_or_ack() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.owner_retired.store(true, Ordering::Release);
        let observed = ledger
            .clone()
            .release_with_observation("peer")
            .await
            .unwrap();
        assert_eq!(
            observed,
            ReleaseObservation::default(),
            "owner death is not an observed physical disconnect receipt"
        );
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.terminal_facts_len(), 0);
        assert_eq!(ledger.maintenance_len(), 0);
        assert!(client.calls.lock().unwrap().is_empty());
        assert!(client.acknowledgments.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn still_live_refusing_owner_remains_owned_until_confirmed_death() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.receipts.lock().unwrap().push_back(Err(failed(
            "spoofed ServiceUnknown from still live owner",
        )
        .with_platform(PlatformDetail::new(
            "bluez-dbus",
            "org.freedesktop.DBus.Error.ServiceUnknown",
        ))));
        assert!(ledger.clone().release("peer").await.is_err());
        assert_eq!(ledger.len(), 1);
        client.owner_retired.store(true, Ordering::Release);
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn owner_query_failure_never_retires_daemon_obligations() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.owner_retired.store(true, Ordering::Release);
        client.owner_query_failed.store(true, Ordering::Release);
        assert!(ledger.clone().release("peer").await.is_err());
        assert_eq!(ledger.len(), 1);
        client.owner_query_failed.store(false, Ordering::Release);
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn cancelled_reservation_keeps_late_token_owned_and_compensates() {
        let ledger = Ledger::default();
        let client = Client::new();
        let task = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        task.abort();
        settled().await;
        assert_eq!(ledger.len(), 1);
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        settled().await;
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, None)]);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn release_answer_preserves_exact_observed_reason_before_retirement() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: Some(2),
        }));
        assert_eq!(
            ledger
                .clone()
                .release_with_observation("peer")
                .await
                .unwrap()
                .disconnect_reason,
            Some(2)
        );
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.terminal_facts_len(), 1);
        assert!(ledger.physical_lost_observed("peer", 73, 2).await);
        assert_eq!(ledger.terminal_facts_len(), 0);
    }

    #[tokio::test]
    #[cfg(feature = "btleplug")]
    async fn pending_discovery_cleanup_cannot_starve_owned_release_or_its_retained_receipt() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: Some(2),
        }));
        let gate = Arc::new(Semaphore::new(0));
        *client.release_gate.lock().unwrap() = Some(gate.clone());
        let owner = ledger.clone();
        let waiter = tokio::spawn(async move {
            crate::btleplug_backend::release_linux_link(std::future::pending(), async move {
                let receipt = owner.release_with_observation("peer").await?;
                Ok(crate::boundary::DisconnectObservation {
                    physical_generation: receipt.physical_generation,
                    platform: receipt
                        .disconnect_reason
                        .map(crate::boundary::bluez_disconnect_observation),
                    cleanup_failure: None,
                })
            })
            .await
        });
        settled().await;
        // Cancelling the caller must not cancel the already-admitted native
        // release, even though the independent discovery answer is still held.
        waiter.abort();
        gate.add_permits(1);
        settled().await;
        assert_eq!(
            ledger.len(),
            0,
            "pending discovery did not starve ReleaseLease"
        );
        assert_eq!(ledger.terminal_facts_len(), 1);
        assert_eq!(client.calls.lock().unwrap().len(), 1);
        let owner_queries = client.owner_queries.load(Ordering::Relaxed);
        client.owner_query_failed.store(true, Ordering::Release);
        let observation = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            crate::btleplug_backend::release_linux_link(std::future::pending(), async {
                let receipt = ledger.clone().release_with_observation("peer").await?;
                Ok(crate::boundary::DisconnectObservation {
                    physical_generation: receipt.physical_generation,
                    platform: receipt
                        .disconnect_reason
                        .map(crate::boundary::bluez_disconnect_observation),
                    cleanup_failure: None,
                })
            }),
        )
        .await
        .expect("pending discovery cannot delay the retained physical answer")
        .unwrap();
        assert_eq!(observation.physical_generation, Some(73));
        assert_eq!(
            observation.platform,
            Some(crate::boundary::bluez_disconnect_observation(2))
        );
        assert!(
            observation.cleanup_failure.is_some(),
            "pending independent cleanup remains visible"
        );
        assert_eq!(
            client.calls.lock().unwrap().len(),
            1,
            "retry consumed the real retained receipt, not a new release"
        );
        assert_eq!(client.owner_queries.load(Ordering::Relaxed), owner_queries);
    }

    #[tokio::test]
    async fn cancelled_release_waiter_preserves_late_terminal_fact_not_native_debt() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: Some(2),
        }));
        let gate = Arc::new(Semaphore::new(0));
        *client.release_gate.lock().unwrap() = Some(gate.clone());
        let owner = ledger.clone();
        let waiter = tokio::spawn(async move { owner.release_with_observation("peer").await });
        settled().await;
        waiter.abort();
        gate.add_permits(1);
        settled().await;
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.terminal_facts_len(), 1);
        let owner_queries = client.owner_queries.load(Ordering::Relaxed);
        client.owner_query_failed.store(true, Ordering::Release);
        assert_eq!(
            ledger
                .clone()
                .release_with_observation("peer")
                .await
                .unwrap(),
            ReleaseObservation {
                physical_generation: Some(73),
                disconnect_reason: Some(2),
            }
        );
        assert_eq!(client.owner_queries.load(Ordering::Relaxed), owner_queries);
        assert!(ledger.physical_lost_observed("peer", 73, 2).await);
        assert!(!ledger.physical_lost_observed("peer", 73, 2).await);
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn new_peer_admission_cannot_consume_a_previous_generation_terminal_fact() {
        let ledger = Ledger::default();
        let first = Client::new();
        first.reserve.add_permits(1);
        first.connect.add_permits(1);
        ledger.clone().connect("peer".into(), first).await.unwrap();
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(ledger.terminal_facts_len(), 1);
        let mut next = Client::new();
        next.generation = 74;
        next.token = 42;
        next.reserve.add_permits(1);
        next.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), next.clone())
            .await
            .unwrap();
        assert_eq!(ledger.terminal_facts_len(), 0);
        assert!(!ledger.physical_lost_observed("peer", 73, 2).await);
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.with_release_scope("peer", Some(73), || panic!("old cleanup must not run")),
            None::<()>
        );
        assert!(next.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_release_cleanup_holds_admission_until_its_synchronous_mutation_finishes() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger.clone().connect("peer".into(), client).await.unwrap();
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(
            ledger.with_release_scope("peer", Some(73), || {
                assert!(
                    ledger.entries.try_lock().is_err(),
                    "admission cannot cross scoped cleanup"
                );
                1
            }),
            Some(1)
        );
    }

    #[tokio::test]
    async fn unknown_native_reason_is_not_inferred_from_requested_release() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger.clone().connect("peer".into(), client).await.unwrap();
        assert_eq!(
            ledger
                .release_with_observation("peer")
                .await
                .unwrap()
                .disconnect_reason,
            None
        );
    }

    #[tokio::test]
    async fn reservation_receipt_cannot_fabricate_physical_disconnect_reason() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        *client.connect_failure.lock().unwrap() = Some(failed("connect refused"));
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 0,
            scope: Scope::Reservation,
            disconnect_reason: Some(2),
        }));
        assert!(ledger.clone().connect("peer".into(), client).await.is_err());
        settled().await;
        assert_eq!(
            ledger.len(),
            1,
            "malformed release stays owned for exact retry"
        );
    }

    #[tokio::test]
    async fn duplicate_peer_refusal_does_not_allocate_unowned_sender_nonces() {
        let ledger = Ledger::default();
        let client = Client::new();
        let task = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        assert_eq!(client.allocations.load(Ordering::Relaxed), 1);
        for _ in 0..8 {
            assert!(
                ledger
                    .clone()
                    .connect("peer".into(), client.clone())
                    .await
                    .is_err()
            );
        }
        assert_eq!(client.allocations.load(Ordering::Relaxed), 1);
        assert_eq!(ledger.len(), 1);
        task.abort();
        client.reserve.add_permits(1);
        settled().await;
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn cancelled_connect_retains_physical_generation_and_owner_client() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        let task = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        task.abort();
        client.connect.add_permits(1);
        settled().await;
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, Some(73))]);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn matching_protected_release_retires_only_this_lease() {
        let owner_a = Ledger::default();
        let owner_b = Ledger::default();
        let client_a = Client::new();
        let mut client_b = Client::new();
        client_b.token = 42;
        client_a.reserve.add_permits(1);
        client_a.connect.add_permits(1);
        client_b.reserve.add_permits(1);
        client_b.connect.add_permits(1);
        owner_a
            .clone()
            .connect("peer".into(), client_a.clone())
            .await
            .unwrap();
        owner_b
            .clone()
            .connect("peer".into(), client_b.clone())
            .await
            .unwrap();
        client_a.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Protected,
            disconnect_reason: None,
        }));
        assert_eq!(
            owner_a
                .clone()
                .release_with_observation("peer")
                .await
                .unwrap(),
            ReleaseObservation::default()
        );
        assert_eq!(owner_a.len(), 0);
        assert_eq!(owner_a.terminal_facts_len(), 0);
        assert!(owner_a.retry_maintenance().await.is_empty());
        assert_eq!(owner_a.maintenance_len(), 0);
        assert_eq!(*client_a.acknowledgments.lock().unwrap(), vec![41]);
        assert_eq!(owner_b.len(), 1);
        assert!(!owner_a.physical_lost("peer", 73).await);
        client_b.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 42,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: None,
        }));
        assert_eq!(
            owner_b
                .clone()
                .release_with_observation("peer")
                .await
                .unwrap()
                .physical_generation,
            Some(73)
        );
        assert!(!owner_a.physical_lost("peer", 73).await);
        assert!(owner_b.physical_lost("peer", 73).await);
    }

    #[tokio::test]
    async fn protected_release_ack_failure_remains_owned_without_repeating_link_release() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        client
            .acknowledgment_failures
            .lock()
            .unwrap()
            .push_back(failed("protected ACK refused"));
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Protected,
            disconnect_reason: None,
        }));
        assert_eq!(
            ledger
                .clone()
                .release_with_observation("peer")
                .await
                .unwrap(),
            ReleaseObservation::default()
        );
        settled().await;
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.terminal_facts_len(), 0);
        assert_eq!(ledger.maintenance_len(), 1);
        assert!(ledger.retry_maintenance().await.is_empty());
        assert_eq!(ledger.maintenance_len(), 0);
        assert_eq!(*client.acknowledgments.lock().unwrap(), vec![41, 41]);
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn protected_indeterminate_and_wrong_generation_remain_retryable() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        for scope in [Scope::Indeterminate, Scope::Reservation] {
            client.receipts.lock().unwrap().push_back(Ok(Receipt {
                token: 41,
                generation: 73,
                scope,
                disconnect_reason: None,
            }));
            assert!(ledger.clone().release("peer").await.is_err());
            assert_eq!(ledger.len(), 1);
        }
        for receipt in [
            Receipt {
                token: 41,
                generation: 0,
                scope: Scope::Protected,
                disconnect_reason: None,
            },
            Receipt {
                token: 41,
                generation: 73,
                scope: Scope::Protected,
                disconnect_reason: Some(2),
            },
            Receipt {
                token: 99,
                generation: 73,
                scope: Scope::Protected,
                disconnect_reason: None,
            },
        ] {
            client.receipts.lock().unwrap().push_back(Ok(receipt));
            assert!(ledger.clone().release("peer").await.is_err());
            assert_eq!(ledger.len(), 1);
        }
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 42,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: None,
        }));
        assert!(ledger.clone().release("peer").await.is_err());
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 74,
            scope: Scope::Physical,
            disconnect_reason: None,
        }));
        assert!(ledger.clone().release("peer").await.is_err());
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(ledger.len(), 0);
        assert!(
            client
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| *call == (41, Some(73)))
        );
    }

    #[tokio::test]
    async fn failed_connect_learns_only_exact_token_physical_release_and_retains_refusal() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        *client.connect_failure.lock().unwrap() =
            Some(failed("connect reply failed after native admission"));
        client
            .receipts
            .lock()
            .unwrap()
            .push_back(Err(failed("old unique owner disappeared")));
        assert!(
            ledger
                .clone()
                .connect("peer".into(), client.clone())
                .await
                .is_err()
        );
        settled().await;
        assert_eq!(ledger.len(), 1);
        assert!(
            ledger
                .clone()
                .connect("peer".into(), Client::new())
                .await
                .is_err()
        );
        client.receipts.lock().unwrap().push_back(Ok(Receipt {
            token: 41,
            generation: 73,
            scope: Scope::Physical,
            disconnect_reason: None,
        }));
        ledger.clone().release("peer").await.unwrap();
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, None), (41, None)]);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn held_acquisition_release_waiter_cancellation_does_not_lose_owned_work() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        let acquired = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        assert_eq!(ledger.peers(), vec!["peer"]);
        let release_ledger = ledger.clone();
        let released = tokio::spawn(async move { release_ledger.release("peer").await });
        settled().await;
        released.abort();
        client.connect.add_permits(1);
        acquired.await.unwrap().unwrap();
        settled().await;
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, Some(73))]);
        assert_eq!(ledger.len(), 0);
        ledger.release("peer").await.unwrap();
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reserve_reply_failure_retains_indeterminate_admission_without_creating_replacement() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        *client.reserve_failure.lock().unwrap() = Some(failed("accepted reserve reply was lost"));
        assert!(
            ledger
                .clone()
                .connect("peer".into(), client.clone())
                .await
                .is_err()
        );
        settled().await;
        assert_eq!(ledger.len(), 1);
        assert!(ledger.clone().release("peer").await.is_err());
        assert_eq!(ledger.len(), 1);
        assert!(client.calls.lock().unwrap().is_empty());
        assert!(ledger.connect("peer".into(), Client::new()).await.is_err());
    }

    #[tokio::test]
    async fn duplicate_old_loss_never_releases_new_token_or_physical_generation() {
        let ledger = Ledger::default();
        let first = Client::new();
        first.reserve.add_permits(1);
        first.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), first.clone())
            .await
            .unwrap();
        assert!(ledger.physical_lost("peer", 73).await);
        ledger.physical_lost("peer", 73).await;
        settled().await;
        assert_eq!(first.calls.lock().unwrap().len(), 1);
        let mut next = Client::new();
        next.token = 42;
        next.generation = 74;
        next.reserve.add_permits(1);
        next.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), next.clone())
            .await
            .unwrap();
        assert!(!ledger.physical_lost("peer", 73).await);
        assert!(!ledger.physical_lost("peer", 0).await);
        settled().await;
        assert!(next.calls.lock().unwrap().is_empty());
        assert_eq!(ledger.len(), 1);
        assert!(ledger.physical_lost("peer", 74).await);
        settled().await;
        assert_eq!(*next.calls.lock().unwrap(), vec![(42, Some(74))]);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn lost_reserve_reply_recovers_original_nonce_and_token_without_reconnect() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        *client.reserve_failure.lock().unwrap() =
            Some(failed("malformed accepted reservation reply"));
        client.recovery.lock().unwrap().push_back(Ok(Some(41)));
        assert!(
            ledger
                .clone()
                .connect("peer".into(), client.clone())
                .await
                .is_err()
        );
        settled().await;
        assert_eq!(ledger.len(), 0);
        let calls = client.reservation_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].0);
        assert!(calls[1].0);
        assert_eq!(calls[0].1, calls[1].1);
        assert_ne!(calls[0].1, 0);
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, None)]);
        assert_eq!(client.connect.available_permits(), 0);
    }

    #[tokio::test]
    async fn authoritative_no_admission_fence_retires_unknown_reservation() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        *client.reserve_failure.lock().unwrap() =
            Some(failed("reservation request rejected before admission"));
        client.recovery.lock().unwrap().push_back(Ok(None));
        assert!(
            ledger
                .clone()
                .connect("peer".into(), client.clone())
                .await
                .is_err()
        );
        settled().await;
        assert_eq!(ledger.len(), 0);
        assert!(client.calls.lock().unwrap().is_empty());
        assert_eq!(client.reservation_calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn vanished_owner_retires_ack_debt_without_repeating_physical_release() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        client
            .acknowledgment_failures
            .lock()
            .unwrap()
            .push_back(failed("ack refused"));
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        ledger.clone().release("peer").await.unwrap();
        settled().await;
        assert_eq!(ledger.maintenance_len(), 1);
        client.owner_retired.store(true, Ordering::Release);
        assert!(ledger.retry_maintenance().await.is_empty());
        assert_eq!(ledger.maintenance_len(), 0);
        assert_eq!(*client.acknowledgments.lock().unwrap(), vec![41]);
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn acknowledgment_failure_is_retryable_housekeeping_not_physical_failure() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        client
            .acknowledgment_failures
            .lock()
            .unwrap()
            .push_back(failed("ack reply lost"));
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        ledger.clone().release("peer").await.unwrap();
        settled().await;
        assert_eq!(ledger.len(), 0, "physical release remains confirmed");
        assert_eq!(ledger.maintenance_len(), 1);
        assert!(ledger.retry_maintenance().await.is_empty());
        assert_eq!(ledger.maintenance_len(), 0);
        assert_eq!(*client.acknowledgments.lock().unwrap(), vec![41, 41]);
        assert_eq!(
            client.calls.lock().unwrap().len(),
            1,
            "no repeated physical release"
        );
    }

    #[tokio::test]
    async fn physical_loss_is_immediate_while_native_release_is_held() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        client.connect.add_permits(1);
        ledger
            .clone()
            .connect("peer".into(), client.clone())
            .await
            .unwrap();
        let gate = Arc::new(Semaphore::new(0));
        *client.release_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let releasing = ledger.clone();
        let task = tokio::spawn(async move { releasing.release("peer").await });
        settled().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                ledger.physical_lost("peer", 73)
            )
            .await
            .unwrap()
        );
        assert!(
            !ledger.physical_lost("peer", 73).await,
            "one winning public loss"
        );
        let unrelated = Client::new();
        unrelated.reserve.add_permits(1);
        unrelated.connect.add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            ledger.clone().connect("other".into(), unrelated),
        )
        .await
        .unwrap()
        .unwrap();
        gate.add_permits(1);
        task.await.unwrap().unwrap();
        settled().await;
        assert_eq!(client.calls.lock().unwrap().len(), 1);
        assert_eq!(ledger.peers(), vec!["other"]);
    }

    #[tokio::test]
    async fn sequential_reconnections_do_not_accumulate_consumed_terminal_debt() {
        let ledger = Ledger::default();
        for identity in 1..=1100 {
            let mut client = Client::new();
            client.token = identity;
            client.generation = identity;
            client.reserve.add_permits(1);
            client.connect.add_permits(1);
            ledger.clone().connect("peer".into(), client).await.unwrap();
            ledger.clone().release("peer").await.unwrap();
            assert!(ledger.retry_maintenance().await.is_empty());
            assert_eq!(ledger.len(), 0);
            assert_eq!(ledger.maintenance_len(), 0);
        }
    }

    #[tokio::test]
    async fn early_losses_replay_exact_matching_generation_without_overwrite() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        let acquired = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        for (generation, reason) in [(73, 3), (74, 1), (72, 0)] {
            assert!(
                !ledger
                    .physical_lost_observed("peer", generation, reason)
                    .await
            );
        }
        client.connect.add_permits(1);
        acquired.await.unwrap().unwrap();
        assert_eq!(*client.replayed_losses.lock().unwrap(), vec![(73, 3)]);
        assert!(ledger.physical_lost_observed("peer", 73, 3).await);
        settled().await;
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn early_loss_intake_overflow_is_explicit_and_compensates_acquisition() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        let acquired = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        for generation in 1..=65 {
            assert!(!ledger.physical_lost("peer", generation).await);
        }
        client.connect.add_permits(1);
        let error = acquired.await.unwrap().unwrap_err();
        assert!(error.detail().unwrap().contains("overflow"));
        settled().await;
        assert_eq!(ledger.len(), 0);
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, Some(73))]);
    }

    #[tokio::test]
    async fn retired_or_full_event_sink_refuses_late_acquisition_and_compensates() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(1);
        *client.replay_failure.lock().unwrap() = Some(failed("event owner is closed or full"));
        let acquired = tokio::spawn(ledger.clone().connect("peer".into(), client.clone()));
        settled().await;
        assert!(!ledger.physical_lost("peer", 73).await);
        client.connect.add_permits(1);
        assert!(acquired.await.unwrap().is_err());
        settled().await;
        assert_eq!(ledger.len(), 0);
        assert_eq!(*client.calls.lock().unwrap(), vec![(41, Some(73))]);
    }

    #[tokio::test]
    async fn new_connection_retries_failed_ack_without_an_admission_wait() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(2);
        client.connect.add_permits(2);
        client
            .acknowledgment_failures
            .lock()
            .unwrap()
            .push_back(failed("transient ack failure"));
        ledger
            .clone()
            .connect("first".into(), client.clone())
            .await
            .unwrap();
        ledger.clone().release("first").await.unwrap();
        settled().await;
        assert_eq!(ledger.maintenance_len(), 1);
        ledger
            .clone()
            .connect("second".into(), client.clone())
            .await
            .unwrap();
        settled().await;
        assert_eq!(ledger.maintenance_len(), 0);
        assert_eq!(*client.acknowledgments.lock().unwrap(), vec![41, 41]);
    }

    #[tokio::test]
    async fn repeated_bounded_cleanup_observers_share_one_held_ack_attempt() {
        let ledger = Ledger::default();
        let client = Client::new();
        client.reserve.add_permits(2);
        client.connect.add_permits(2);
        let gate = Arc::new(Semaphore::new(0));
        *client.acknowledgment_gate.lock().unwrap() = Some(Arc::clone(&gate));
        ledger
            .clone()
            .connect("first".into(), client.clone())
            .await
            .unwrap();
        ledger.clone().release("first").await.unwrap();
        settled().await;
        for _ in 0..4 {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(5),
                    ledger.retry_maintenance()
                )
                .await
                .is_err()
            );
        }
        settled().await;
        assert!(
            ledger.maintenance_owners() <= 3,
            "cancelled observers must not accumulate retained waiters"
        );
        ledger
            .clone()
            .connect("second".into(), client.clone())
            .await
            .unwrap();
        assert_eq!(client.acknowledgments.lock().unwrap().len(), 1);
        gate.add_permits(1);
        settled().await;
        assert_eq!(ledger.maintenance_len(), 0);
    }
}
