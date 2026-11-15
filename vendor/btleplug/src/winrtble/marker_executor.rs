// The operation owner outlives every application Tokio runtime. Initialize it
// before initiating a native operation, so startup failure cannot orphan one.
pub(super) fn handle() -> Result<&'static tokio::runtime::Handle, &'static str> {
    static OWNER: std::sync::OnceLock<Result<tokio::runtime::Handle, &'static str>> = std::sync::OnceLock::new();
    OWNER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new().name("ubm-winrt-operation-owner".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
                Ok(runtime) => runtime,
                Err(_) => { let _ = sender.send(Err("WinRT operation owner runtime initialization failed")); return; }
            };
            if sender.send(Ok(runtime.handle().clone())).is_ok() {
                runtime.block_on(std::future::pending::<()>());
            }
        }).map_err(|_| "WinRT operation owner thread creation failed")?;
        receiver.recv().map_err(|_| "WinRT operation owner thread exited during initialization")?
    }).as_ref().map_err(|error| *error)
}

// The host owns graph operations even after the requesting runtime is dropped.
pub(super) fn spawn_owned<F>(future: F) -> Result<tokio::task::JoinHandle<F::Output>, &'static str>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(handle()?.spawn(future))
}

// A native Close can block indefinitely. Give each cleanup its own thread so
// neither the operation runtime nor another peer's cleanup depends on it.
pub(super) async fn run_blocking_owned<F, T>(work: F) -> Result<T, &'static str>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new().name("ubm-winrt-cleanup".into()).spawn(move || {
        let _ = sender.send(work());
    }).map_err(|_| "WinRT cleanup thread creation failed")?;
    receiver.await.map_err(|_| "WinRT cleanup thread exited without a result")
}

pub(super) fn try_graph_exclusive(
    graph: &tokio::sync::RwLock<()>,
) -> Result<tokio::sync::RwLockWriteGuard<'_, ()>, &'static str> {
    graph.try_write().map_err(|_| "WinRT graph operation is pending; retry")
}

// `None` includes an unavailable status: it is not a terminal fence.
pub(super) async fn wait_for_terminal<T>(mut observe: impl FnMut() -> Option<T>) -> T {
    loop {
        if let Some(result) = observe() { return result; }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use std::time::Duration;

    #[test]
    fn blocking_cleanup_does_not_block_other_peer_or_release_its_owner_early() {
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let lease = lifecycle.clone().try_write_owned().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (other_tx, other_rx) = std::sync::mpsc::channel();
        super::handle().unwrap().spawn(async move {
            let _lease = lease;
            super::run_blocking_owned(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }).await.unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        super::handle().unwrap().spawn(async move { other_tx.send(()).unwrap(); });
        let other_progressed = other_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let retained = lifecycle.try_write().is_err();
        release_tx.send(()).unwrap();
        assert!(other_progressed, "blocked cleanup stalled another peer");
        assert!(retained, "cleanup released its owner before native return");
    }

    #[test]
    fn graph_exclusive_admission_returns_promptly_while_native_work_is_owned() {
        let graph = tokio::sync::RwLock::new(());
        let operation = graph.try_read().unwrap();
        assert!(super::try_graph_exclusive(&graph).is_err());
        drop(operation);
        let discovery = super::try_graph_exclusive(&graph).unwrap();
        assert!(graph.try_read().is_err());
        assert!(super::try_graph_exclusive(&graph).is_err());
        drop(discovery);
        assert!(super::try_graph_exclusive(&graph).is_ok());
    }

    #[test]
    fn graph_operations_remain_owned_after_caller_shutdown() {
        {
            let caller = tokio::runtime::Builder::new_current_thread().build().unwrap();
            let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
            let lease = lifecycle.clone().try_read_owned().unwrap();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            caller.block_on(async move {
                super::spawn_owned(async move {
                    let _lease = lease;
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    done_tx.send(()).unwrap();
                }).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(caller);
            assert!(lifecycle.try_write().is_err(), "owned operation lost its lease");
            finish_tx.send(()).unwrap();
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            for _ in 0..200 {
                if lifecycle.try_write().is_ok() { break; }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(lifecycle.try_write().is_ok(), "owned operation did not release its lease");
        }
    }

    #[test]
    fn terminal_fence_waits_through_unavailable_and_pending_status() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observe = checks.clone();
            let result = super::wait_for_terminal(move || {
                match observe.fetch_add(1, Ordering::SeqCst) {
                    0 | 1 => None,
                    _ => Some("terminal, remote effect unknown"),
                }
            }).await;
            assert_eq!(result, "terminal, remote effect unknown");
            assert_eq!(checks.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn reply_owner_survives_requesting_runtime_shutdown_before_and_after_poll() {
        for poll_before_shutdown in [false, true] {
            let caller = tokio::runtime::Builder::new_current_thread().build().unwrap();
            let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
            let lease = lifecycle.clone().try_read_owned().unwrap();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
            let (allow_tx, allow_rx) = tokio::sync::oneshot::channel::<()>();
            let mut allow_tx = Some(allow_tx);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let done = Arc::new(AtomicBool::new(false));
            let finished = done.clone();
            if poll_before_shutdown { allow_tx.take().unwrap().send(()).unwrap(); }
            caller.block_on(async move {
                super::handle().unwrap().spawn(async move {
                    let _lease = lease;
                    allow_rx.await.unwrap();
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    finished.store(true, Ordering::SeqCst);
                });
            });
            if poll_before_shutdown { started_rx.recv_timeout(Duration::from_secs(2)).unwrap(); }
            else { assert!(started_rx.try_recv().is_err()); }
            drop(caller);
            assert!(lifecycle.try_write().is_err());
            if !poll_before_shutdown {
                allow_tx.take().unwrap().send(()).unwrap();
                started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            finish_tx.send(()).unwrap();
            for _ in 0..200 {
                if done.load(Ordering::SeqCst) && lifecycle.try_write().is_ok() { break; }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(done.load(Ordering::SeqCst));
            assert!(lifecycle.try_write().is_ok());
        }
    }
}
