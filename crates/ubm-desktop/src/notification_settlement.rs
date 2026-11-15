//! Activation-scoped ownership for native notification operations.
//!
//! This module is a foundation, not a radio dispatcher. Its keys must contain
//! the complete captured activation and admission identities. Integration must
//! coordinate the first-poll gate with the ticket and GATT starting locks, and
//! reconcile the actual native answer against the exact core identity before
//! returning an outcome. The executor must remain alive for late completion.
use std::{
    collections::{HashMap, VecDeque},
    future::{Future, poll_fn},
    hash::Hash,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify},
    time::Instant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "The settlement state vocabulary is retained for central integration routes compiled outside this module."
)]
pub enum RegistryError {
    Capacity,
    Duplicate,
    UnknownConsumer,
    Pending,
    Unacknowledged,
    PendingDrain,
    DeliveryIndeterminate,
    InvalidState,
    Retired,
}
#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Retained item ownership is exercised through the central settlement integration."
)]
pub struct RetainError<V> {
    pub error: RegistryError,
    pub item: V,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    Abandoned,
    Cancelled,
    Deadline,
}
/// Whether a native future has crossed its physical dispatch boundary.
pub enum FirstPoll<T> {
    Waiting,
    Dispatched(Poll<T>),
    Stopped(Stop),
}
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Retired outcomes are preserved for exact native reconciliation."
)]
pub enum Outcome<R, E> {
    Success(R),
    Failed(E),
    Retired(Result<R, E>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State<N, R, E> {
    Prepared {
        attempt: u64,
    },
    NotStarted {
        attempt: u64,
        reason: Stop,
    },
    InFlight {
        attempt: u64,
    },
    Reconciling {
        attempt: u64,
        native: Result<N, E>,
    },
    Complete {
        attempt: u64,
        native: Result<N, E>,
        outcome: Outcome<R, E>,
    },
    Indeterminate {
        attempt: u64,
        native: Option<Result<N, E>>,
        diagnostic: &'static str,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Wait results remain part of the retained settlement protocol."
)]
pub enum WaitResult<N, R, E> {
    Settled(State<N, R, E>),
    TimedOut,
    Cancelled,
}

#[allow(
    dead_code,
    reason = "Tail accounting fields are retained for complete delivery semantics."
)]
struct Tail<V> {
    drain_required: bool,
    item_limit: usize,
    byte_limit: usize,
    bytes: usize,
    items: VecDeque<(V, usize)>,
    delivery: Arc<AsyncMutex<()>>,
    acknowledged: bool,
    delivering: bool,
    delivery_indeterminate: bool,
}
struct SlotData<C, V, N, R, E> {
    state: State<N, R, E>,
    started: bool,
    retired: bool,
    waiters: usize,
    prepared_waker: Option<Waker>,
    tails: HashMap<C, Tail<V>>,
}
pub struct Settlement<C, V, N, R, E> {
    data: Mutex<SlotData<C, V, N, R, E>>,
    changed: Notify,
}
type SettlementSlot<C, V, N, R, E> = Arc<Settlement<C, V, N, R, E>>;

struct RegistryData<A, C, V, N, R, E> {
    slots: HashMap<A, SettlementSlot<C, V, N, R, E>>,
    reserved_consumers: usize,
    reserved_items: usize,
    reserved_bytes: usize,
}
pub struct Registry<A, C, V, N, R, E> {
    data: Mutex<RegistryData<A, C, V, N, R, E>>,
    capacity: usize,
    item_capacity: usize,
    byte_capacity: usize,
}

impl<A: Eq + Hash, C: Eq + Hash, V, N, R, E> Registry<A, C, V, N, R, E> {
    pub fn new(capacity: usize, item_capacity: usize, byte_capacity: usize) -> Self {
        Self {
            data: Mutex::new(RegistryData {
                slots: HashMap::new(),
                reserved_consumers: 0,
                reserved_items: 0,
                reserved_bytes: 0,
            }),
            capacity,
            item_capacity,
            byte_capacity,
        }
    }
    /// Reserve both activation and tail budget before any core mutation or effect.
    /// The capacity bounds both activation records and total consumer records.
    pub fn reserve(
        &self,
        activation: A,
        consumer: C,
        drain_required: bool,
        items: usize,
        bytes: usize,
    ) -> Result<SettlementSlot<C, V, N, R, E>, RegistryError> {
        let mut data = self.data.lock().unwrap();
        if data.slots.contains_key(&activation) {
            return Err(RegistryError::Duplicate);
        }
        if data.slots.len() >= self.capacity
            || data.reserved_consumers >= self.capacity
            || items > self.item_capacity.saturating_sub(data.reserved_items)
            || bytes > self.byte_capacity.saturating_sub(data.reserved_bytes)
        {
            return Err(RegistryError::Capacity);
        }
        let mut tails = HashMap::new();
        tails.insert(
            consumer,
            Tail {
                drain_required,
                item_limit: items,
                byte_limit: bytes,
                bytes: 0,
                items: VecDeque::new(),
                delivery: Arc::new(AsyncMutex::new(())),
                acknowledged: false,
                delivering: false,
                delivery_indeterminate: false,
            },
        );
        let slot = Arc::new(Settlement {
            data: Mutex::new(SlotData {
                state: State::Prepared { attempt: 1 },
                started: false,
                retired: false,
                waiters: 0,
                prepared_waker: None,
                tails,
            }),
            changed: Notify::new(),
        });
        data.reserved_consumers += 1;
        data.reserved_items += items;
        data.reserved_bytes += bytes;
        data.slots.insert(activation, Arc::clone(&slot));
        Ok(slot)
    }
    /// Reserve an additional exact consumer on an existing activation.
    #[allow(
        dead_code,
        reason = "Multi-consumer settlement remains covered through central integration."
    )]
    pub fn add_consumer(
        &self,
        activation: &A,
        consumer: C,
        drain_required: bool,
        items: usize,
        bytes: usize,
    ) -> Result<(), RegistryError> {
        let mut data = self.data.lock().unwrap();
        let slot = data
            .slots
            .get(activation)
            .cloned()
            .ok_or(RegistryError::Retired)?;
        let mut slot_data = slot.data.lock().unwrap();
        if slot_data.retired {
            return Err(RegistryError::Retired);
        }
        if slot_data.tails.contains_key(&consumer) {
            return Err(RegistryError::Duplicate);
        }
        if !matches!(
            slot_data.state,
            State::Prepared { .. } | State::InFlight { .. } | State::Reconciling { .. }
        ) {
            return Err(RegistryError::InvalidState);
        }
        if data.reserved_consumers >= self.capacity
            || items > self.item_capacity.saturating_sub(data.reserved_items)
            || bytes > self.byte_capacity.saturating_sub(data.reserved_bytes)
        {
            return Err(RegistryError::Capacity);
        }
        slot_data.tails.insert(
            consumer,
            Tail {
                drain_required,
                item_limit: items,
                byte_limit: bytes,
                bytes: 0,
                items: VecDeque::new(),
                delivery: Arc::new(AsyncMutex::new(())),
                acknowledged: false,
                delivering: false,
                delivery_indeterminate: false,
            },
        );
        data.reserved_consumers += 1;
        data.reserved_items += items;
        data.reserved_bytes += bytes;
        Ok(())
    }
    pub fn get(&self, activation: &A) -> Option<SettlementSlot<C, V, N, R, E>> {
        self.data.lock().unwrap().slots.get(activation).cloned()
    }
    /// Never evict unresolved work, unacknowledged receipts, or retained data.
    pub fn retire(&self, activation: &A) -> Result<(), RegistryError> {
        let mut data = self.data.lock().unwrap();
        let slot = data
            .slots
            .get(activation)
            .cloned()
            .ok_or(RegistryError::Retired)?;
        let mut s = slot.data.lock().unwrap();
        if !matches!(s.state, State::Complete { .. } | State::NotStarted { .. }) || s.waiters != 0 {
            return Err(RegistryError::Pending);
        }
        if s.tails
            .values()
            .any(|t| !t.acknowledged || !t.items.is_empty() || t.delivering)
        {
            return Err(RegistryError::Unacknowledged);
        }
        for tail in s.tails.values() {
            data.reserved_items -= tail.item_limit;
            data.reserved_bytes -= tail.byte_limit;
        }
        data.reserved_consumers -= s.tails.len();
        s.retired = true;
        data.slots.remove(activation);
        Ok(())
    }
}

#[allow(
    dead_code,
    reason = "Private settlement operations are reached through central integration rather than this standalone module target."
)]
impl<C: Eq + Hash, V, N: Clone, R: Clone, E: Clone> Settlement<C, V, N, R, E> {
    pub fn snapshot(&self) -> State<N, R, E> {
        self.data.lock().unwrap().state.clone()
    }
    fn attempt(&self) -> u64 {
        match self.snapshot() {
            State::Prepared { attempt }
            | State::NotStarted { attempt, .. }
            | State::InFlight { attempt }
            | State::Reconciling { attempt, .. }
            | State::Complete { attempt, .. }
            | State::Indeterminate { attempt, .. } => attempt,
        }
    }
    /// Pre-dispatch cancellation shares the gate with the native first poll.
    pub fn cancel_prepared(&self, attempt: u64) {
        self.stop_prepared(attempt, Stop::Cancelled);
    }
    fn stop_prepared(&self, expected: u64, reason: Stop) {
        let mut s = self.data.lock().unwrap();
        if let State::Prepared { attempt } = s.state {
            if attempt != expected {
                return;
            }
            s.state = State::NotStarted { attempt, reason };
            let waker = s.prepared_waker.take();
            drop(s);
            if let Some(waker) = waker {
                waker.wake();
            }
            self.changed.notify_waiters();
        }
    }
    /// On rejection the caller retains ownership of the observation.
    /// The supplied size must account for the complete retained allocation.
    pub fn retain(&self, consumer: &C, item: V, bytes: usize) -> Result<(), RetainError<V>> {
        let mut s = self.data.lock().unwrap();
        if s.retired {
            return Err(RetainError {
                error: RegistryError::Retired,
                item,
            });
        }
        let Some(tail) = s.tails.get_mut(consumer) else {
            return Err(RetainError {
                error: RegistryError::UnknownConsumer,
                item,
            });
        };
        if !tail.drain_required || tail.acknowledged {
            return Err(RetainError {
                error: RegistryError::InvalidState,
                item,
            });
        }
        if tail.items.len() >= tail.item_limit || bytes > tail.byte_limit.saturating_sub(tail.bytes)
        {
            return Err(RetainError {
                error: RegistryError::Capacity,
                item,
            });
        }
        tail.bytes += bytes;
        tail.items.push_back((item, bytes));
        Ok(())
    }
    /// Callback execution is serialized and holds no registry mutex. A panic
    /// cannot prove whether the callback's side effect occurred.
    pub async fn deliver(
        &self,
        consumer: &C,
        callback: Option<&(dyn Fn(V) + Send + Sync)>,
    ) -> Result<(), RegistryError> {
        let delivery = {
            let s = self.data.lock().unwrap();
            if s.retired {
                return Err(RegistryError::Retired);
            }
            Arc::clone(
                &s.tails
                    .get(consumer)
                    .ok_or(RegistryError::UnknownConsumer)?
                    .delivery,
            )
        };
        let _lease = delivery.lock().await;
        let _delivery = DeliveryGuard::new(self, consumer)?;
        loop {
            let item = {
                let mut s = self.data.lock().unwrap();
                let tail = s
                    .tails
                    .get_mut(consumer)
                    .ok_or(RegistryError::UnknownConsumer)?;
                if callback.is_none() && !tail.items.is_empty() {
                    return Err(RegistryError::PendingDrain);
                }
                let item = tail.items.pop_front();
                if let Some((_, bytes)) = &item {
                    tail.bytes -= *bytes;
                }
                item
            };
            match item {
                Some((value, _)) => callback.expect("checked before pop")(value),
                None => return Ok(()),
            }
        }
    }
    pub fn acknowledge(&self, consumer: &C) -> Result<(), RegistryError> {
        let mut s = self.data.lock().unwrap();
        if s.retired {
            return Err(RegistryError::Retired);
        }
        if !matches!(s.state, State::Complete { .. } | State::NotStarted { .. }) {
            return Err(RegistryError::Pending);
        }
        let tail = s
            .tails
            .get_mut(consumer)
            .ok_or(RegistryError::UnknownConsumer)?;
        if tail.delivery_indeterminate {
            return Err(RegistryError::DeliveryIndeterminate);
        }
        if !tail.items.is_empty() || tail.delivering {
            return Err(RegistryError::PendingDrain);
        }
        tail.acknowledged = true;
        Ok(())
    }
    /// Retry requires an observed failure (or a known undispatched attempt).
    /// Indeterminate work never authorizes a duplicate physical operation.
    pub fn prepare_retry(&self) -> Result<(), RegistryError> {
        let mut s = self.data.lock().unwrap();
        if s.retired {
            return Err(RegistryError::Retired);
        }
        let attempt = match &s.state {
            State::Complete {
                attempt,
                outcome: Outcome::Failed(_),
                ..
            }
            | State::NotStarted { attempt, .. } => *attempt,
            State::Prepared { .. }
            | State::InFlight { .. }
            | State::Reconciling { .. }
            | State::Indeterminate { .. } => return Err(RegistryError::Pending),
            _ => return Err(RegistryError::InvalidState),
        };
        if s.waiters != 0
            || s.tails
                .values()
                .any(|t| !t.acknowledged || !t.items.is_empty() || t.delivering)
        {
            return Err(RegistryError::Unacknowledged);
        }
        let next = attempt.checked_add(1).ok_or(RegistryError::Capacity)?;
        for tail in s.tails.values_mut() {
            tail.acknowledged = false;
        }
        s.state = State::Prepared { attempt: next };
        s.started = false;
        drop(s);
        self.changed.notify_waiters();
        Ok(())
    }
    pub async fn wait_dispatched(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !matches!(self.snapshot(), State::Prepared { .. }) {
                return;
            }
            notified.await;
        }
    }
    pub async fn wait(&self) -> State<N, R, E> {
        let _waiter = Waiter::new(self);
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.snapshot();
            if matches!(
                state,
                State::NotStarted { .. } | State::Complete { .. } | State::Indeterminate { .. }
            ) {
                return state;
            }
            notified.await;
        }
    }
    pub async fn wait_until(&self, deadline: Instant) -> WaitResult<N, R, E> {
        let attempt = self.attempt();
        if Instant::now() >= deadline {
            self.stop_prepared(attempt, Stop::Deadline);
            return WaitResult::TimedOut;
        }
        tokio::select! { biased; _ = async { if Instant::now() < deadline { tokio::time::sleep_until(deadline).await; } } => { self.stop_prepared(attempt, Stop::Deadline); WaitResult::TimedOut }, result = self.wait() => WaitResult::Settled(result) }
    }
    pub async fn wait_or_cancel<F: Future<Output = ()>>(
        &self,
        deadline: Instant,
        cancelled: F,
    ) -> WaitResult<N, R, E> {
        let attempt = self.attempt();
        tokio::select! { biased; _ = cancelled => { self.stop_prepared(attempt, Stop::Cancelled); WaitResult::Cancelled }, _ = async { if Instant::now() < deadline { tokio::time::sleep_until(deadline).await; } } => { self.stop_prepared(attempt, Stop::Deadline); WaitResult::TimedOut }, result = self.wait() => WaitResult::Settled(result) }
    }
}
#[allow(
    dead_code,
    reason = "Delivery cleanup remains part of the exact settlement lifecycle."
)]
struct DeliveryGuard<'a, C: Eq + Hash, V, N, R, E> {
    slot: &'a Settlement<C, V, N, R, E>,
    consumer: &'a C,
}
#[allow(
    dead_code,
    reason = "Delivery guard construction is used by the retained settlement lifecycle."
)]
impl<'a, C: Eq + Hash, V, N, R, E> DeliveryGuard<'a, C, V, N, R, E> {
    fn new(slot: &'a Settlement<C, V, N, R, E>, consumer: &'a C) -> Result<Self, RegistryError> {
        let mut s = slot.data.lock().unwrap();
        if s.retired {
            return Err(RegistryError::Retired);
        }
        let tail = s
            .tails
            .get_mut(consumer)
            .ok_or(RegistryError::UnknownConsumer)?;
        if tail.delivery_indeterminate {
            return Err(RegistryError::DeliveryIndeterminate);
        }
        tail.delivering = true;
        Ok(Self { slot, consumer })
    }
}
impl<C: Eq + Hash, V, N, R, E> Drop for DeliveryGuard<'_, C, V, N, R, E> {
    fn drop(&mut self) {
        if let Some(tail) = self.slot.data.lock().unwrap().tails.get_mut(self.consumer) {
            tail.delivering = false;
            if std::thread::panicking() {
                tail.delivery_indeterminate = true;
            }
        }
    }
}
struct Waiter<'a, C, V, N, R, E> {
    slot: &'a Settlement<C, V, N, R, E>,
}
impl<'a, C, V, N, R, E> Waiter<'a, C, V, N, R, E> {
    fn new(slot: &'a Settlement<C, V, N, R, E>) -> Self {
        slot.data.lock().unwrap().waiters += 1;
        Self { slot }
    }
}
impl<C, V, N, R, E> Drop for Waiter<'_, C, V, N, R, E> {
    fn drop(&mut self) {
        self.slot.data.lock().unwrap().waiters -= 1;
    }
}

/// Dropping the initiating caller synchronously prevents a still-prepared poll.
pub struct CallerGuard<C, V, N, R, E> {
    slot: Arc<Settlement<C, V, N, R, E>>,
    attempt: u64,
}
impl<C, V, N, R, E> Drop for CallerGuard<C, V, N, R, E> {
    fn drop(&mut self) {
        let mut s = self.slot.data.lock().unwrap();
        if let State::Prepared { attempt } = s.state {
            if attempt != self.attempt {
                return;
            }
            s.state = State::NotStarted {
                attempt,
                reason: Stop::Abandoned,
            };
            let waker = s.prepared_waker.take();
            drop(s);
            if let Some(waker) = waker {
                waker.wake();
            }
            self.slot.changed.notify_waiters();
        }
    }
}
struct TaskGuard<C, V, N, R, E> {
    slot: Arc<Settlement<C, V, N, R, E>>,
    attempt: u64,
}
impl<C, V, N, R, E> Drop for TaskGuard<C, V, N, R, E> {
    fn drop(&mut self) {
        let mut s = self.slot.data.lock().unwrap_or_else(|e| e.into_inner());
        let attempt = match &s.state {
            State::Prepared { attempt }
            | State::InFlight { attempt }
            | State::Reconciling { attempt, .. } => *attempt,
            _ => return,
        };
        if attempt != self.attempt {
            return;
        }
        let old = std::mem::replace(
            &mut s.state,
            State::Indeterminate {
                attempt,
                native: None,
                diagnostic: "settlement task ended without a reconciled answer",
            },
        );
        if let State::Reconciling { native, .. } = old {
            s.state = State::Indeterminate {
                attempt,
                native: Some(native),
                diagnostic: "settlement task ended without a reconciled answer",
            };
        }
        drop(s);
        self.slot.changed.notify_waiters();
    }
}
#[allow(
    dead_code,
    reason = "The settlement executor is reached through central integration routes."
)]
impl<C, V, N, R, E> Settlement<C, V, N, R, E>
where
    C: Eq + Hash + Send + 'static,
    V: Send + 'static,
    N: Clone + Send + 'static,
    R: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    /// Publish before spawning. The factory and first poll run under the slot
    /// gate; integration must additionally serialize its ticket/admission gate.
    pub fn start<F, Fut, Reconcile, ReconcileFuture>(
        self: &Arc<Self>,
        deadline: Instant,
        factory: F,
        reconcile: Reconcile,
    ) -> Result<CallerGuard<C, V, N, R, E>, RegistryError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<N, E>> + Send + 'static,
        Reconcile: FnOnce(Result<N, E>) -> ReconcileFuture + Send + 'static,
        ReconcileFuture: Future<Output = Outcome<R, E>> + Send + 'static,
    {
        self.start_controlled(
            deadline,
            factory,
            |mut future, cx| FirstPoll::Dispatched(future.as_mut().poll(cx)),
            reconcile,
        )
    }

    /// Keep the attempt prepared while a backend's cancellable pre-dispatch
    /// gate is pending. The gate runs under the slot lock and must serialize
    /// cancellation with the single poll that crosses the native boundary.
    pub fn start_controlled<F, Fut, Gate, Reconcile, ReconcileFuture>(
        self: &Arc<Self>,
        deadline: Instant,
        factory: F,
        mut gate: Gate,
        reconcile: Reconcile,
    ) -> Result<CallerGuard<C, V, N, R, E>, RegistryError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<N, E>> + Send + 'static,
        Gate: FnMut(Pin<&mut Fut>, &mut Context<'_>) -> FirstPoll<Result<N, E>> + Send + 'static,
        Reconcile: FnOnce(Result<N, E>) -> ReconcileFuture + Send + 'static,
        ReconcileFuture: Future<Output = Outcome<R, E>> + Send + 'static,
    {
        let attempt = {
            let mut s = self.data.lock().unwrap();
            if s.retired {
                return Err(RegistryError::Retired);
            }
            if s.started {
                return Err(RegistryError::Pending);
            }
            let State::Prepared { attempt } = s.state else {
                return Err(RegistryError::Pending);
            };
            s.started = true;
            attempt
        };
        let guard = CallerGuard {
            slot: Arc::clone(self),
            attempt,
        };
        let task_guard = TaskGuard {
            slot: Arc::clone(self),
            attempt,
        };
        let slot = Arc::clone(self);
        tokio::spawn(async move {
            let _task_guard = task_guard;
            let mut factory = Some(factory);
            let mut future: Option<Pin<Box<Fut>>> = None;
            let mut deadline_sleep = Box::pin(tokio::time::sleep_until(deadline));
            let mut dispatched = false;
            let answer = poll_fn(|cx| {
                if dispatched {
                    let future = future.as_mut().expect("dispatched future exists");
                    return future.as_mut().poll(cx).map(Some);
                }
                let mut s = slot.data.lock().unwrap();
                let attempt = match s.state {
                    State::Prepared { attempt: current } if current == attempt => current,
                    _ => return Poll::Ready(None),
                };
                if Instant::now() >= deadline {
                    s.state = State::NotStarted {
                        attempt,
                        reason: Stop::Deadline,
                    };
                    drop(s);
                    slot.changed.notify_waiters();
                    return Poll::Ready(None);
                }
                // Unwind outside the mutex so a panicking native first poll
                // cannot poison the registry or hide the indeterminate receipt.
                let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let native = future
                        .get_or_insert_with(|| Box::pin(factory.take().expect("created once")()));
                    gate(native.as_mut(), cx)
                }));
                let result = match first {
                    Ok(value) => value,
                    Err(panic) => {
                        drop(s);
                        std::panic::resume_unwind(panic)
                    }
                };
                match result {
                    FirstPoll::Waiting => {
                        s.prepared_waker = Some(cx.waker().clone());
                        if deadline_sleep.as_mut().poll(cx).is_ready() {
                            cx.waker().wake_by_ref();
                        }
                        Poll::Pending
                    }
                    FirstPoll::Stopped(reason) => {
                        s.state = State::NotStarted { attempt, reason };
                        s.prepared_waker = None;
                        drop(s);
                        slot.changed.notify_waiters();
                        Poll::Ready(None)
                    }
                    FirstPoll::Dispatched(result) => {
                        s.state = State::InFlight { attempt };
                        s.prepared_waker = None;
                        dispatched = true;
                        drop(s);
                        slot.changed.notify_waiters();
                        result.map(Some)
                    }
                }
            })
            .await;
            let Some(native) = answer else {
                return;
            };
            let attempt = {
                let mut s = slot.data.lock().unwrap();
                let State::InFlight { attempt } = s.state else {
                    return;
                };
                s.state = State::Reconciling {
                    attempt,
                    native: native.clone(),
                };
                attempt
            };
            slot.changed.notify_waiters();
            let outcome = reconcile(native.clone()).await;
            let mut s = slot.data.lock().unwrap();
            if matches!(s.state, State::Reconciling { attempt: current, .. } if current == attempt)
            {
                s.state = State::Complete {
                    attempt,
                    native,
                    outcome,
                };
            }
            drop(s);
            slot.changed.notify_waiters();
        });
        Ok(guard)
    }
}
