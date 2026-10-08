//! Stage ownership shared by the WinRT adapter and deterministic tests.

use crate::errors::DesktopError;
#[cfg(test)]
use crate::errors::{PlatformDetail, PlatformValue};
use std::sync::{Mutex, PoisonError, TryLockError};

/// Exact device owners survive logical physical loss until native retirement confirms release.
pub(crate) struct DeviceOwners<T> {
    owners: tokio::sync::Mutex<std::collections::HashMap<String, (T, bool)>>,
}
impl<T> Default for DeviceOwners<T> {
    fn default() -> Self {
        Self {
            owners: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}
impl<T: Clone> DeviceOwners<T> {
    pub async fn retain(&self, peer: &str, owner: T) -> Result<(), DesktopError> {
        let mut owners = self.owners.lock().await;
        if owners.contains_key(peer) {
            return Err(DesktopError::connection_failed(
                "previous native device cleanup remains owned",
            ));
        }
        owners.insert(peer.to_owned(), (owner, false));
        Ok(())
    }
    pub async fn retire(&self, peer: &str) {
        if let Some((_, retired)) = self.owners.lock().await.get_mut(peer) {
            *retired = true;
        }
    }
    pub async fn peers(&self) -> Vec<String> {
        self.owners.lock().await.keys().cloned().collect()
    }
    pub async fn release<F, Fut>(
        &self,
        peer: &str,
        terminal_only: bool,
        release: F,
    ) -> Result<(), DesktopError>
    where
        F: FnOnce(T) -> Fut,
        Fut: std::future::Future<Output = Result<(), DesktopError>>,
    {
        let mut owners = self.owners.lock().await;
        let Some((owner, retired)) = owners.get(peer) else {
            return Ok(());
        };
        if terminal_only && !*retired {
            return Ok(());
        }
        release(owner.clone()).await?;
        owners.remove(peer);
        Ok(())
    }
}

/// A transient native read never adopts connection ownership. Close is
/// synchronous before another await, including when inspection failed; both
/// failures are retained rather than allowing one to hide the other.
pub(crate) fn inspect_transient<T>(
    read: impl FnOnce() -> Result<T, DesktopError>,
    close: impl FnOnce() -> Result<(), DesktopError>,
) -> Result<T, DesktopError> {
    let read = read();
    let close = close();
    match (read, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(read), Err(close)) => {
            cleanup_result(vec![read, close]).and_then(|()| unreachable!("nonempty failure list"))
        }
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;
    #[tokio::test]
    async fn physical_loss_keeps_exact_native_discovery_owner_until_natural_completion() {
        let owners = DeviceOwners::default();
        owners.retain("peer", 17).await.unwrap();
        owners.retire("peer").await;
        assert!(
            owners
                .release("peer", true, |owner| async move {
                    assert_eq!(owner, 17);
                    Err(DesktopError::connection_failed("discovery still pending"))
                })
                .await
                .is_err()
        );
        assert!(owners.retain("peer", 18).await.is_err());
        assert_eq!(owners.peers().await, ["peer"]);
        owners
            .release("peer", true, |owner| async move {
                assert_eq!(owner, 17);
                Ok(())
            })
            .await
            .unwrap();
        assert!(owners.peers().await.is_empty());
        owners.retain("peer", 18).await.unwrap();
        owners
            .release("peer", true, |_| async {
                panic!("terminal cleanup must not release a fresh active generation")
            })
            .await
            .unwrap();
        assert_eq!(owners.peers().await, ["peer"]);
        owners
            .release("peer", false, |owner| async move {
                assert_eq!(owner, 18);
                Ok(())
            })
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn failed_handler_cleanup_does_not_skip_independent_peripheral_release() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let mut stages = CleanupStages::new([true; 3]);
        let mut attempted = Vec::new();
        let failed = release_session_stages(&mut stages, |stage| {
            attempted.push(stage);
            if stage == 0 {
                Err(DesktopError::connection_failed(
                    "handler removal retained for retry",
                ))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            attempted,
            [0, 1, 2],
            "maintain disable and session Close were attempted and succeeded"
        );
        let result = release_independent(cleanup_result(failed), async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            result
                .unwrap_err()
                .detail()
                .unwrap()
                .contains("retained for retry")
        );
        let mut retried = Vec::new();
        assert!(
            release_session_stages(&mut stages, |stage| {
                retried.push(stage);
                Ok::<_, DesktopError>(())
            })
            .is_empty()
        );
        assert_eq!(
            retried,
            [0],
            "the failed handler remains owned; confirmed sibling stages are not retried"
        );
    }
    #[test]
    fn transient_inspection_closes_after_success_or_validation_failure() {
        use std::cell::Cell;
        for refuse in [false, true] {
            let closed = Cell::new(false);
            let result = inspect_transient(
                || {
                    if refuse {
                        Err(DesktopError::connection_failed("native identity mismatch"))
                    } else {
                        Ok(7)
                    }
                },
                || {
                    closed.set(true);
                    Ok(())
                },
            );
            assert!(closed.get());
            assert_eq!(result.is_err(), refuse);
        }
    }
    #[test]
    fn transient_inspection_retains_read_and_close_failures() {
        let result: Result<(), _> = inspect_transient(
            || Err(DesktopError::connection_failed("validation failed")),
            || Err(DesktopError::connection_failed("close failed")),
        );
        let error = result.unwrap_err();
        let metadata = &error.platform().expect("both native failures").metadata;
        assert!(
            metadata
                .values()
                .any(|value| *value == PlatformValue::Text("validation failed".into()))
        );
        assert!(
            metadata
                .values()
                .any(|value| *value == PlatformValue::Text("close failed".into()))
        );
    }
}

/// Independent owned native stages must both be attempted. A failed stage's
/// owner remains retryable; a successful sibling cannot erase that failure.
pub(crate) async fn release_independent(
    maintained: Result<(), DesktopError>,
    peripheral: impl std::future::Future<Output = Result<(), DesktopError>>,
) -> Result<(), DesktopError> {
    let peripheral = peripheral.await;
    cleanup_result(
        maintained
            .err()
            .into_iter()
            .chain(peripheral.err())
            .collect(),
    )
}

pub(crate) struct PeerAdmission {
    slots: Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<bool>>>>,
    closing: std::sync::atomic::AtomicBool,
}

impl PeerAdmission {
    pub(crate) fn new() -> Self {
        Self {
            slots: Mutex::new(std::collections::HashMap::new()),
            closing: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn slot(&self, peer: &str) -> std::sync::Arc<tokio::sync::Mutex<bool>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(peer.into())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(false)))
            .clone()
    }
    pub(crate) async fn acquire(
        &self,
        peer: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<bool>, ()> {
        if self.closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(());
        }
        let guard = self.slot(peer).lock_owned().await;
        if self.closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(());
        }
        Ok(guard)
    }
    pub(crate) fn release(&self, peer: &str) -> Result<tokio::sync::OwnedMutexGuard<bool>, ()> {
        self.slot(peer).try_lock_owned().map_err(|_| ())
    }
    pub(crate) fn close(&self) -> Vec<String> {
        self.closing
            .store(true, std::sync::atomic::Ordering::Release);
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

pub(crate) struct RetryVault<T> {
    scopes: Mutex<std::collections::BTreeMap<String, std::sync::Arc<RetryScope<T>>>>,
}

struct RetryScope<T> {
    owners: Mutex<Vec<T>>,
    admission: Mutex<()>,
}

impl<T> RetryVault<T> {
    pub(crate) const fn new() -> Self {
        Self {
            scopes: Mutex::new(std::collections::BTreeMap::new()),
        }
    }
    fn scope(&self, scope: &str) -> std::sync::Arc<RetryScope<T>> {
        self.scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(scope.to_owned())
            .or_insert_with(|| {
                std::sync::Arc::new(RetryScope {
                    owners: Mutex::new(Vec::new()),
                    admission: Mutex::new(()),
                })
            })
            .clone()
    }
    pub(crate) fn push(&self, scope: &str, owner: T) {
        self.scope(scope)
            .owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(owner);
    }

    /// Reentrant/concurrent retry reports busy, never a false clean snapshot.
    /// The native callback runs outside the ownership mutex; reentrant callers
    /// use try_lock and therefore never wait for that callback's own return.
    pub(crate) fn retry<E>(
        &self,
        scope: &str,
        release: impl FnMut(&mut T) -> Vec<E>,
    ) -> Result<Vec<E>, ()> {
        let scope = self.scope(scope);
        let _admission = match scope.admission.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(()),
        };
        let mut owners =
            std::mem::take(&mut *scope.owners.lock().unwrap_or_else(PoisonError::into_inner));
        let failures = retain_failed(&mut owners, release);
        let mut retained = scope.owners.lock().unwrap_or_else(PoisonError::into_inner);
        retained.extend(owners);
        if failures.is_empty() && !retained.is_empty() {
            return Err(());
        }
        Ok(failures)
    }
}

/// Serializes callback effects with shutdown admission. Callback work must
/// never block on the event queue; close waits only for an admitted callback.
pub(crate) struct CallbackGate(Mutex<bool>);

impl CallbackGate {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(false))
    }
    pub(crate) fn run<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let closed = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *closed { None } else { Some(action()) }
    }
    pub(crate) fn close(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }
}

/// Singular operation boundaries carry all individual errors under indexed
/// platform metadata. Shutdown reports retain the individual typed values.
pub(crate) fn cleanup_result(failures: Vec<DesktopError>) -> Result<(), DesktopError> {
    crate::errors::cleanup_result("winrt", failures)
}

#[derive(Clone)]
pub(crate) struct CleanupStages<const N: usize> {
    pending: [bool; N],
}

impl<const N: usize> CleanupStages<N> {
    pub(crate) fn new(pending: [bool; N]) -> Self {
        Self { pending }
    }

    pub(crate) fn activate(&mut self, stage: usize) {
        self.pending[stage] = true;
    }

    /// Try every pending stage, retaining failures independently. Success
    /// removes only that obligation, so already-revoked event tokens are
    /// never retried as if they were still registered.
    pub(crate) fn run<E>(&mut self, mut attempt: impl FnMut(usize) -> Result<(), E>) -> Vec<E> {
        let mut failures = Vec::new();
        for (stage, pending) in self.pending.iter_mut().enumerate() {
            if *pending {
                match attempt(stage) {
                    Ok(()) => *pending = false,
                    Err(error) => failures.push(error),
                }
            }
        }
        failures
    }
}

/// Stages: revoke service handler, disable MaintainConnection, close session.
/// A confirmed Close authoritatively retires the disable obligation even
/// when disabling first failed; that earlier failure remains in this report.
pub(crate) fn release_session_stages<E>(
    stages: &mut CleanupStages<3>,
    attempt: impl FnMut(usize) -> Result<(), E>,
) -> Vec<E> {
    let failures = stages.run(attempt);
    if !stages.pending[2] {
        stages.pending[1] = false;
    }
    failures
}

/// Retain the actual owner, not only an error string, when any stage fails.
pub(crate) fn retain_failed<T, E>(
    owned: &mut Vec<T>,
    mut release: impl FnMut(&mut T) -> Vec<E>,
) -> Vec<E> {
    let mut failures = Vec::new();
    owned.retain_mut(|owner| {
        let errors = release(owner);
        let retain = !errors.is_empty();
        failures.extend(errors);
        retain
    });
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_vault_retry_cannot_look_clean_to_another_caller() {
        let vault = std::sync::Arc::new(RetryVault::new());
        vault.push("A", 7);
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker_vault = vault.clone();
        let worker = std::thread::spawn(move || {
            worker_vault.retry("A", |owner| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                vec![*owner]
            })
        });
        entered_rx.recv().unwrap();
        assert_eq!(vault.retry("A", |_| Vec::<usize>::new()), Err(()));
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Ok(vec![7]));
        assert_eq!(vault.retry("A", |_| Vec::<usize>::new()), Ok(vec![]));
    }

    #[test]
    fn held_and_refused_adapter_a_does_not_block_or_taint_adapter_b() {
        let vault = std::sync::Arc::new(RetryVault::new());
        vault.push("A", 7);
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker_vault = vault.clone();
        let worker = std::thread::spawn(move || {
            worker_vault.retry("A", |owner| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                vec![(*owner, "remove refused"), (*owner, "stop refused")]
            })
        });
        entered_rx.recv().unwrap();
        let same_scope = vault.retry("A", |_| Vec::<(usize, &str)>::new());
        let unrelated_open = vault.retry("B", |_| panic!("B cannot attempt A cleanup"));
        vault.push("B", 9);
        let unrelated_close = vault.retry("B", |owner| {
            assert_eq!(*owner, 9);
            Vec::<(usize, &str)>::new()
        });
        release.send(()).unwrap();
        let refused = worker.join().unwrap();
        assert_eq!(same_scope, Err(()));
        assert_eq!(unrelated_open, Ok(Vec::<(usize, &str)>::new()));
        assert_eq!(unrelated_close, Ok(vec![]));
        assert_eq!(
            refused,
            Ok(vec![(7, "remove refused"), (7, "stop refused")])
        );
        assert_eq!(
            vault.retry("B", |_| panic!("A refusal cannot taint B")),
            Ok(Vec::<(usize, &str)>::new())
        );
        assert_eq!(
            vault.retry("A", |owner| {
                assert_eq!(*owner, 7);
                Vec::<(usize, &str)>::new()
            }),
            Ok(vec![])
        );
        assert_eq!(
            vault.retry("A", |_| panic!("successful retry retires A")),
            Ok(Vec::<(usize, &str)>::new())
        );
    }

    #[tokio::test]
    async fn peer_acquisition_reuses_healthy_owner_and_close_refuses_new_work() {
        let slots = std::sync::Arc::new(PeerAdmission::new());
        let mut first = slots.acquire("peer").await.unwrap();
        let second_slots = slots.clone();
        let second = tokio::spawn(async move {
            let healthy = second_slots.acquire("peer").await.unwrap();
            assert!(*healthy, "a second lease reuses the first owner");
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        assert!(
            slots.release("peer").is_err(),
            "teardown must report in-flight acquire"
        );
        *first = true;
        drop(first);
        second.await.unwrap();
        assert_eq!(slots.close(), vec!["peer"]);
        assert!(slots.acquire("peer").await.is_err());
        let mut released = slots.release("peer").unwrap();
        *released = false;
        drop(released);
        assert!(!*slots.release("peer").unwrap());
    }

    #[test]
    fn callback_queued_behind_close_has_no_effect_and_full_queue_does_not_park() {
        let gate = CallbackGate::new();
        gate.close();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        sender.try_send(1).unwrap();
        assert!(gate.run(|| sender.try_send(2)).is_none());
        assert_eq!(receiver.try_recv().unwrap(), 1);
        let open = CallbackGate::new();
        sender.try_send(3).unwrap();
        assert!(open.run(|| sender.try_send(4)).unwrap().is_err());
        open.close();
    }

    #[tokio::test]
    async fn cancelling_a_queued_acquisition_preserves_the_current_peer_owner() {
        let slots = std::sync::Arc::new(PeerAdmission::new());
        let mut current = slots.acquire("peer").await.unwrap();
        *current = true;
        let queued_slots = slots.clone();
        let queued = tokio::spawn(async move { queued_slots.acquire("peer").await });
        tokio::task::yield_now().await;
        assert!(!queued.is_finished());
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());
        assert!(*current, "cancellation cannot retire another lease's owner");
        drop(current);
        let retained = slots.release("peer").unwrap();
        assert!(*retained);
        drop(retained);
        assert!(slots.acquire("unrelated-peer").await.is_ok());
    }

    #[tokio::test]
    async fn close_rejects_an_already_queued_acquisition_when_the_owner_settles() {
        let slots = std::sync::Arc::new(PeerAdmission::new());
        let current = slots.acquire("peer").await.unwrap();
        let queued_slots = slots.clone();
        let queued = tokio::spawn(async move { queued_slots.acquire("peer").await });
        tokio::task::yield_now().await;
        assert!(!queued.is_finished());
        assert_eq!(slots.close(), vec!["peer"]);
        drop(current);
        assert!(queued.await.unwrap().is_err());
        assert!(
            slots.release("peer").is_ok(),
            "cleanup remains admitted after close"
        );
    }

    #[test]
    fn close_fences_a_held_callback_before_returning() {
        let gate = std::sync::Arc::new(CallbackGate::new());
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let callback_gate = gate.clone();
        let callback = std::thread::spawn(move || {
            callback_gate.run(|| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let close_gate = gate.clone();
        let (closing, closing_rx) = std::sync::mpsc::channel();
        let (closed, closed_rx) = std::sync::mpsc::channel();
        let close = std::thread::spawn(move || {
            closing.send(()).unwrap();
            close_gate.close();
            closed.send(()).unwrap();
        });
        closing_rx.recv().unwrap();
        assert!(closed_rx.try_recv().is_err());
        release.send(()).unwrap();
        callback.join().unwrap();
        close.join().unwrap();
        assert!(
            gate.run(|| panic!("closed callback must not run"))
                .is_none()
        );
    }

    #[test]
    fn singular_receipt_retains_each_native_failure_and_stage() {
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        let errors = (0..3)
            .map(|stage| {
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Platform,
                    format!("stage.{stage}"),
                )
                .with_platform(
                    PlatformDetail::new("winrt", "hresult")
                        .with_metadata("hresult", PlatformValue::Int(stage)),
                )
            })
            .collect();
        let failure = cleanup_result(errors).unwrap_err();
        let platform = failure.platform().unwrap();
        for stage in 0..3 {
            assert_eq!(
                platform
                    .metadata
                    .get(&format!("failure.{stage}.platform.metadata.hresult")),
                Some(&PlatformValue::Int(stage))
            );
            assert_eq!(
                platform.metadata.get(&format!("failure.{stage}.operation")),
                Some(&PlatformValue::Text(format!("stage.{stage}")))
            );
        }
    }

    #[test]
    fn partial_registration_keeps_later_tokens_and_failed_compensation_for_retry() {
        let mut stages = CleanupStages::new([false; 6]);
        for stage in [0, 2, 4, 5] {
            stages.activate(stage);
        }
        let mut owners = vec![stages];
        let failures = retain_failed(&mut owners, |stages| {
            stages.run(|stage| if stage == 2 { Err(stage) } else { Ok(()) })
        });
        assert_eq!(failures, vec![2]);
        assert_eq!(owners.len(), 1);
        let mut retried = Vec::new();
        assert!(
            retain_failed(&mut owners, |stages| stages.run(|stage| {
                retried.push(stage);
                Ok::<_, usize>(())
            }))
            .is_empty()
        );
        assert_eq!(retried, vec![2]);
        assert!(owners.is_empty());
    }

    #[test]
    fn every_refused_watch_stage_is_the_only_stage_retried() {
        for refused in 0..6 {
            let mut stages = CleanupStages::new([false; 6]);
            for stage in 0..6 {
                stages.activate(stage);
            }
            let failures = stages.run(|stage| if stage == refused { Err(stage) } else { Ok(()) });
            assert_eq!(failures, vec![refused]);
            let mut retried = Vec::new();
            assert!(
                stages
                    .run(|stage| {
                        retried.push(stage);
                        Ok::<_, usize>(())
                    })
                    .is_empty()
            );
            assert_eq!(retried, vec![refused]);
            assert!(
                stages
                    .run::<usize>(|_| panic!("confirmed cleanup is not repeated"))
                    .is_empty()
            );
        }
    }

    #[test]
    fn session_attempts_close_after_disable_refusal_and_preserves_all_failures() {
        let mut stages = CleanupStages::new([true; 3]);
        assert_eq!(
            release_session_stages(&mut stages, Err::<(), _>),
            vec![0, 1, 2]
        );
        let mut calls = Vec::new();
        assert!(
            release_session_stages(&mut stages, |stage| {
                calls.push(stage);
                Ok::<_, usize>(())
            })
            .is_empty()
        );
        assert_eq!(calls, vec![0, 1, 2]);
    }

    #[test]
    fn confirmed_session_close_settles_disable_but_keeps_its_diagnostic() {
        let mut stages = CleanupStages::new([true; 3]);
        assert_eq!(
            release_session_stages(&mut stages, |stage| if stage == 1 {
                Err(stage)
            } else {
                Ok(())
            }),
            vec![1]
        );
        assert!(
            release_session_stages::<usize>(&mut stages, |_| panic!(
                "closed session must not disable again"
            ))
            .is_empty()
        );
    }

    #[test]
    fn failed_old_owner_survives_replacement_and_successful_retry_removes_only_it() {
        let mut owned = vec![1, 2];
        assert_eq!(
            retain_failed(&mut owned, |value| if *value == 1 {
                vec![*value]
            } else {
                vec![]
            }),
            vec![1]
        );
        owned.push(3);
        assert_eq!(owned, vec![1, 3]);
        assert!(retain_failed(&mut owned, |_| Vec::<usize>::new()).is_empty());
        assert!(owned.is_empty());
    }
}
