//! Admission tickets keep queued WinRT callbacks alive through retirement.
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(super) struct CallbackRetirement {
    state: Mutex<State>,
    changed: Notify,
}
struct State {
    open: bool,
    pending: usize,
}
pub(super) struct Ticket(Arc<CallbackRetirement>);

impl CallbackRetirement {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                open: true,
                pending: 0,
            }),
            changed: Notify::new(),
        }
    }
    pub(super) fn admit(self: &Arc<Self>) -> Option<Ticket> {
        let mut state = self.state.lock().unwrap();
        if !state.open {
            return None;
        }
        state.pending = state
            .pending
            .checked_add(1)
            .expect("callback ticket count exhausted");
        Some(Ticket(self.clone()))
    }
    pub(super) fn close(&self) {
        self.state.lock().unwrap().open = false;
    }
    pub(super) fn open(&self) {
        self.state.lock().unwrap().open = true;
    }
    pub(super) async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().pending == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.pending -= 1;
        let empty = state.pending == 0;
        drop(state);
        if empty {
            self.0.changed.notify_waiters();
        }
    }
}

/// Wait for the peer's ordered overflow cut without missing a concurrent drain.
pub(super) async fn wait_for_peer_fence<E>(
    changed: &Notify,
    mut complete: impl FnMut() -> std::result::Result<bool, E>,
) -> std::result::Result<(), E> {
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if complete()? {
            return Ok(());
        }
        notified.await;
    }
}

/// Enqueue only after both lifetimes admit the callback. The publisher drops
/// the pair after publication and before releasing its consumer wake hold.
pub(super) fn admit_managed(
    global: &Arc<CallbackRetirement>,
    activation: &Arc<CallbackRetirement>,
    enqueue: impl FnOnce(Box<dyn FnOnce() + Send>),
    publish: impl FnOnce((Ticket, Ticket)) + Send + 'static,
) {
    let Some(global_ticket) = global.admit() else {
        return;
    };
    let Some(ticket) = activation.admit() else {
        return;
    };
    enqueue(Box::new(move || publish((global_ticket, ticket))));
}

/// A route fence shares the callback queue's admission order. Returning true
/// means that the cut is retained; its downstream fence acknowledges execution.
pub(super) fn enqueue_retirement(
    enqueue: impl FnOnce(Box<dyn FnOnce() + Send>) -> bool,
    retire: impl FnOnce() + Send + 'static,
) -> bool {
    enqueue(Box::new(retire))
}

pub(crate) fn recover_matching<T, E>(
    owners: &mut Vec<T>,
    mut matches: impl FnMut(&T) -> bool,
    mut close: impl FnMut(&mut T) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    let mut index = 0;
    while index < owners.len() {
        if matches(&owners[index]) {
            close(&mut owners[index])?;
            owners.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(())
}
