//! Interrupted native queries remain owned until the native operation is terminal.
//! Cancellation requests retirement; they do not prove that service objects can close.
//! Queries whose native cancellation is not quiescent retain their original operation
//! without cancellation and refuse close until natural completion.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) trait NativeOperation: Clone {
    type Error: std::fmt::Display;
    fn pending(&self) -> Result<bool, Self::Error>;
    fn cancel(&self) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetirementError<E> {
    Native(E),
    Pending,
}

pub(crate) type RetirementFailures<E> = Vec<(&'static str, RetirementError<E>)>;

struct Entry<T: NativeOperation> {
    operation: T,
    stage: &'static str,
    failure: Option<T::Error>,
    cancel_on_retirement: bool,
}

struct PoolInner<T: NativeOperation> {
    next: AtomicU64,
    entries: Mutex<BTreeMap<u64, Entry<T>>>,
}

pub(crate) struct OperationPool<T: NativeOperation>(Arc<PoolInner<T>>);
impl<T: NativeOperation> Clone for OperationPool<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<T: NativeOperation> Default for OperationPool<T> {
    fn default() -> Self {
        Self(Arc::new(PoolInner {
            next: AtomicU64::new(1),
            entries: Mutex::new(BTreeMap::new()),
        }))
    }
}
impl<T: NativeOperation> OperationPool<T> {
    /// `cancel_on_retirement` is false when native Cancel can publish a terminal
    /// status before the operation releases resources needed by service close.
    pub(crate) fn admit(
        &self,
        operation: T,
        cancel_on_retirement: bool,
        stage: &'static str,
    ) -> OperationGuard<T> {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        self.0
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Entry {
                    operation: operation.clone(),
                    stage,
                    failure: None,
                    cancel_on_retirement,
                },
            );
        OperationGuard {
            pool: self.clone(),
            id: Some(id),
            operation,
            stage,
            cancel_on_retirement,
        }
    }

    /// Report cancellation failures and active operations before allowing close.
    /// Every native call happens outside the registry mutex. A pending operation
    /// survives this refusal so the same connection can retry cleanup later.
    pub(crate) fn retire(&self) -> Result<(), RetirementFailures<T::Error>> {
        let entries: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
            .map(|(id, entry)| {
                (
                    *id,
                    entry.operation.clone(),
                    entry.stage,
                    entry.failure.take(),
                    entry.cancel_on_retirement,
                )
            })
            .collect();
        let mut failures = Vec::new();
        for (id, operation, stage, failure, cancel_on_retirement) in entries {
            if let Some(failure) = failure {
                failures.push((stage, RetirementError::Native(failure)));
            }
            match operation.pending() {
                Ok(false) => {
                    self.0
                        .entries
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                }
                Ok(true) => {
                    if cancel_on_retirement {
                        if let Err(error) = operation.cancel() {
                            failures.push((stage, RetirementError::Native(error)));
                        }
                    }
                    failures.push((stage, RetirementError::Pending));
                }
                Err(error) => failures.push((stage, RetirementError::Native(error))),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }
}

pub(crate) struct OperationGuard<T: NativeOperation> {
    pool: OperationPool<T>,
    id: Option<u64>,
    operation: T,
    stage: &'static str,
    cancel_on_retirement: bool,
}
impl<T: NativeOperation> OperationGuard<T> {
    pub(crate) fn complete(mut self) {
        if let Some(id) = self.id.take() {
            self.pool
                .0
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
    }
}
impl<T: NativeOperation> Drop for OperationGuard<T> {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if !self.cancel_on_retirement {
            return;
        }
        let Err(error) = self.operation.cancel() else {
            return;
        };
        let mut entries = self
            .pool
            .0
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(&id) {
            entry.failure = Some(error);
        } else {
            log::error!(
                "Native {} cancellation failed after registry retirement: {error}",
                self.stage
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Clone)]
    struct Operation {
        pending: Arc<AtomicBool>,
        refuse: Arc<AtomicBool>,
        cancellations: Arc<AtomicUsize>,
    }
    impl Operation {
        fn new() -> Self {
            Self {
                pending: Arc::new(AtomicBool::new(true)),
                refuse: Arc::new(AtomicBool::new(false)),
                cancellations: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl NativeOperation for Operation {
        type Error = &'static str;
        fn pending(&self) -> Result<bool, Self::Error> {
            Ok(self.pending.load(Ordering::SeqCst))
        }
        fn cancel(&self) -> Result<(), Self::Error> {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            if self.refuse.load(Ordering::SeqCst) {
                Err("cancel-refused")
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn non_quiescent_cancellation_does_not_authorize_service_close() {
        let pool = OperationPool::default();
        let operation = Operation::new();
        // WinRT Cancel can mark an operation Canceled while its characteristic
        // initialization still owns the service. Leave the hot query running,
        // retaining its exact owner until natural completion instead.
        drop(pool.admit(operation.clone(), false, "characteristics"));
        assert_eq!(operation.cancellations.load(Ordering::SeqCst), 0);
        for _ in 0..3 {
            assert_eq!(
                pool.retire(),
                Err(vec![("characteristics", RetirementError::Pending)])
            );
        }
        assert_eq!(operation.cancellations.load(Ordering::SeqCst), 0);
        operation.pending.store(false, Ordering::SeqCst);
        assert_eq!(pool.retire(), Ok(()));
        assert_eq!(pool.retire(), Ok(()));
    }

    #[test]
    fn interrupted_query_requests_cancel_but_cannot_close_its_service_until_terminal() {
        let pool = OperationPool::default();
        let operation = Operation::new();
        drop(pool.admit(operation.clone(), true, "characteristics"));
        assert_eq!(operation.cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(
            pool.retire(),
            Err(vec![("characteristics", RetirementError::Pending)])
        );
        operation.pending.store(false, Ordering::SeqCst);
        assert_eq!(pool.retire(), Ok(()));
        assert_eq!(pool.retire(), Ok(()));
    }

    #[test]
    fn successful_query_does_not_cancel_and_does_not_retire_another_pending_query() {
        let pool = OperationPool::default();
        let successful = Operation::new();
        let pending = Operation::new();
        let guard = pool.admit(successful.clone(), true, "services");
        drop(pool.admit(pending.clone(), true, "included-services"));
        guard.complete();
        assert_eq!(successful.cancellations.load(Ordering::SeqCst), 0);
        assert_eq!(
            pool.retire(),
            Err(vec![("included-services", RetirementError::Pending)])
        );
        pending.pending.store(false, Ordering::SeqCst);
        assert_eq!(pool.retire(), Ok(()));
    }

    #[test]
    fn cancel_refusal_is_reported_and_exact_operation_remains_owned_for_retry() {
        let pool = OperationPool::default();
        let operation = Operation::new();
        operation.refuse.store(true, Ordering::SeqCst);
        drop(pool.admit(operation.clone(), true, "descriptors"));
        assert_eq!(
            pool.retire(),
            Err(vec![
                ("descriptors", RetirementError::Native("cancel-refused")),
                ("descriptors", RetirementError::Native("cancel-refused")),
                ("descriptors", RetirementError::Pending),
            ])
        );
        operation.refuse.store(false, Ordering::SeqCst);
        assert_eq!(
            pool.retire(),
            Err(vec![("descriptors", RetirementError::Pending)])
        );
        operation.pending.store(false, Ordering::SeqCst);
        assert_eq!(pool.retire(), Ok(()));
        assert_eq!(operation.cancellations.load(Ordering::SeqCst), 3);
    }
}
