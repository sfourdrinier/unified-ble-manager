use std::sync::RwLock;

// No WinRT registration handoff proves a gapless cut. Publish the uncertain
// terminal before a gate can suppress observations; recovery never adopts it.
#[allow(
    dead_code,
    reason = "Standalone callback-gate fixtures include this shared WinRT module without exercising its separate handoff route."
)]
pub(super) fn terminate_handoff(terminate: impl FnOnce(), fence: impl FnOnce()) {
    terminate();
    fence();
}

pub(super) struct CallbackGate(RwLock<bool>);

impl CallbackGate {
    pub(super) fn new(open: bool) -> Self {
        Self(RwLock::new(open))
    }
    pub(super) fn set(&self, open: bool) {
        *self.0.write().unwrap() = open;
    }
    #[cfg(test)]
    pub(super) fn has_admitted_callback(&self) -> bool {
        self.0.try_write().is_err()
    }
    pub(super) fn admit(&self, callback: impl FnOnce()) {
        let open = self.0.read().unwrap();
        if *open {
            callback();
        }
    }
    pub(super) fn admit_with_delivery<H>(&self, hold: impl FnOnce() -> H, callback: impl FnOnce()) {
        let delivery = hold();
        self.admit(callback);
        drop(delivery);
    }
}

#[cfg(test)]
mod tests {
    use super::CallbackGate;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn callbacks_remain_fenced_through_failed_handoff_and_close() {
        let old = CallbackGate::new(true);
        let candidate = CallbackGate::new(false);
        let delivered = AtomicUsize::new(0);
        let callback = || {
            delivered.fetch_add(1, Ordering::SeqCst);
        };
        old.admit(callback);
        old.set(false); // graph retired; candidate registration may already exist
        old.admit(callback);
        candidate.admit(callback); // failed removal or failed service close
        assert_eq!(delivered.load(Ordering::SeqCst), 1);
        candidate.set(true); // only after a successful retry and publication
        old.admit(callback);
        candidate.admit(callback);
        assert_eq!(delivered.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retirement_waits_for_a_callback_already_admitted() {
        use std::sync::{Arc, mpsc};
        let gate = Arc::new(CallbackGate::new(true));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let callback_gate = gate.clone();
        let callback = std::thread::spawn(move || {
            callback_gate.admit(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        assert!(gate.has_admitted_callback());
        let (retired_tx, retired_rx) = mpsc::channel();
        let retirement = std::thread::spawn(move || {
            gate.set(false);
            retired_tx.send(()).unwrap();
        });
        assert!(retired_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        callback.join().unwrap();
        retirement.join().unwrap();
        retired_rx.recv().unwrap();
    }

    #[test]
    fn synchronous_consumer_wake_can_reenter_retirement() {
        use std::sync::{Arc, atomic::AtomicBool};
        use std::task::{Wake, Waker};

        struct ReentrantWake {
            gate: Arc<CallbackGate>,
            acquired: AtomicBool,
        }
        impl Wake for ReentrantWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.acquired
                    .store(self.gate.0.try_write().is_ok(), Ordering::SeqCst);
            }
        }

        let gate = Arc::new(CallbackGate::new(true));
        let probe = Arc::new(ReentrantWake {
            gate: gate.clone(),
            acquired: AtomicBool::new(false),
        });
        let waker = Waker::from(probe.clone());
        struct WakeOnDrop(Waker);
        impl Drop for WakeOnDrop {
            fn drop(&mut self) {
                self.0.wake_by_ref();
            }
        }
        gate.admit_with_delivery(|| WakeOnDrop(waker), || {});
        assert!(
            probe.acquired.load(Ordering::SeqCst),
            "consumer woke while retirement was excluded by the callback gate"
        );
    }
}
