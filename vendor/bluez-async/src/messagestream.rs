use crate::match_cleanup::{MatchFailure, MatchLease, MatchRegistry};
use dbus::channel::{MatchingReceiver, Token};
use dbus::message::MatchRule;
use dbus::nonblock::SyncConnection;
use dbus::Message;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::mpsc::UnboundedSender;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::sync::Notify;

/// Local callback ownership is distinct from the connection's shared server
/// rule. A missing local token must never be mistaken for a removed bus rule.
pub struct MessageStream {
    token: Token,
    events: UnboundedReceiver<Message>,
    connection: Arc<SyncConnection>,
    lease: Option<MatchLease>,
}

/// Ownership of a logical scan at the D-Bus callback boundary.
///
/// BlueZ does not attach a scan-session identifier to `PropertiesChanged`, so
/// the only truthful association is the epoch active when dbus-rs invokes the
/// local callback.  The handle is deliberately adapter-local: one BlueZ
/// connection can serve several adapters and each owns its own scan lifecycle.
#[derive(Clone)]
pub(crate) struct ScanIngress {
    state: Arc<Mutex<ScanIngressState>>,
    fence_settled: Arc<Notify>,
}

struct ScanIngressState {
    active: Option<u64>,
    retiring: Option<u64>,
    fenced: Option<u64>,
    fence_pending: bool,
    closed: bool,
    sender: Option<UnboundedSender<ScanIngressItem>>,
}

pub(crate) enum ScanIngressItem {
    Message {
        message: Message,
        epoch: Option<u64>,
    },
    Fence {
        epoch: u64,
    },
}

impl ScanIngress {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ScanIngressState {
                active: None,
                retiring: None,
                fenced: None,
                fence_pending: false,
                closed: false,
                sender: None,
            })),
            fence_settled: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn arm(&self, epoch: u64) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("scan ingress state");
        if state.closed {
            return Err("the scan ingress source has closed");
        }
        if state.active.is_some() || state.retiring.is_some() {
            return Err("a scan ingress epoch is already active or draining");
        }
        state.fenced = None;
        state.active = Some(epoch);
        Ok(())
    }

    /// Stop assigning callbacks to `epoch`. Call [`Self::fence`] next to
    /// await the accepted callback prefix at the stream handoff.
    pub(crate) fn retire(&self, epoch: u64) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("scan ingress state");
        if state.active != Some(epoch) {
            return Err("the requested scan ingress epoch is not active");
        }
        state.active = None;
        state.retiring = Some(epoch);
        state.fence_pending = false;
        Ok(())
    }

    pub(crate) async fn fence(&self, epoch: u64) -> Result<(), &'static str> {
        loop {
            let notified = self.fence_settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let marker = {
                let mut state = self.state.lock().expect("scan ingress state");
                if state.fenced == Some(epoch) {
                    return Ok(());
                }
                if state.closed && state.retiring == Some(epoch) {
                    state.retiring = None;
                    state.fenced = Some(epoch);
                    return Ok(());
                }
                if state.active.is_some() || state.retiring != Some(epoch) {
                    return Err("the requested scan ingress epoch is not retiring");
                }
                if state.fence_pending {
                    None
                } else {
                    let sender = state
                        .sender
                        .clone()
                        .ok_or("the scan ingress stream is unavailable")?;
                    state.fence_pending = true;
                    Some(sender)
                }
            };
            if let Some(sender) = marker {
                if sender
                    .unbounded_send(ScanIngressItem::Fence { epoch })
                    .is_err()
                {
                    let mut state = self.state.lock().expect("scan ingress state");
                    if state.retiring == Some(epoch) {
                        state.fence_pending = false;
                    }
                    return Err("the scan ingress stream closed before its fence");
                }
            }
            notified.await;
        }
    }

    fn accept(&self, message: Message) -> bool {
        // Keep the state lock through enqueue: either this callback records
        // the retiring epoch before `retire`, or it is unowned afterwards.
        // Thus the marker `fence` appends after every owned callback.
        let state = self.state.lock().expect("scan ingress state");
        let Some(sender) = state.sender.as_ref() else {
            return false;
        };
        sender
            .unbounded_send(ScanIngressItem::Message {
                message,
                epoch: state.active,
            })
            .is_ok()
    }

    fn install(&self, sender: UnboundedSender<ScanIngressItem>) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("scan ingress state");
        if state.sender.replace(sender).is_some() {
            return Err("the scan ingress stream is already installed");
        }
        Ok(())
    }

    fn uninstall(&self) {
        self.state.lock().expect("scan ingress state").sender = None;
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().expect("scan ingress state");
        state.closed = true;
        state.sender = None;
        self.fence_settled.notify_waiters();
    }

    pub(crate) fn settle_fence(&self, epoch: u64) {
        let mut state = self.state.lock().expect("scan ingress state");
        if state.retiring == Some(epoch) {
            state.retiring = None;
            state.fenced = Some(epoch);
            state.fence_pending = false;
            self.fence_settled.notify_waiters();
        }
    }
}

/// The two BlueZ match rules must share one receiver: `select_all` has no
/// cross-stream order in which a fence can be inserted.  Every callback
/// captures its epoch before this receiver can be delayed or polled later.
pub(crate) struct ScanIngressStream {
    tokens: Vec<Token>,
    events: UnboundedReceiver<ScanIngressItem>,
    connection: Arc<SyncConnection>,
    leases: Vec<MatchLease>,
    ingress: ScanIngress,
}

async fn acquire_scan_rules(
    rules: impl IntoIterator<Item = MatchRule<'static>>,
    registry: &Arc<MatchRegistry>,
    scope: u64,
) -> Result<Vec<(MatchRule<'static>, MatchLease)>, MatchFailure> {
    let mut acquired = Vec::new();
    for rule in rules {
        let lease = registry.acquire(rule.match_str(), scope).await?;
        acquired.push((rule, lease));
    }
    Ok(acquired)
}

impl ScanIngressStream {
    pub(crate) async fn new(
        rules: impl IntoIterator<Item = MatchRule<'static>>,
        connection: Arc<SyncConnection>,
        registry: &Arc<MatchRegistry>,
        scope: u64,
        ingress: ScanIngress,
    ) -> Result<Self, MatchFailure> {
        // Acquire every server match first. If any acquisition fails, the
        // already-owned leases drop through MatchRegistry's retained cleanup
        // path; no local callback or ingress sender has been installed yet.
        let acquired = acquire_scan_rules(rules, registry, scope).await?;
        let (sender, events) = futures::channel::mpsc::unbounded();
        ingress
            .install(sender)
            .map_err(|detail| MatchFailure::new("org.bluez.Error.Failed", detail))?;
        let mut tokens = Vec::new();
        let mut leases = Vec::with_capacity(acquired.len());
        for (rule, lease) in acquired {
            let callback_ingress = ingress.clone();
            let token = connection.start_receive(
                rule,
                Box::new(move |message, _| callback_ingress.accept(message)),
            );
            tokens.push(token);
            leases.push(lease);
        }
        Ok(Self {
            tokens,
            events,
            connection,
            leases,
            ingress,
        })
    }
}

impl Stream for ScanIngressStream {
    type Item = ScanIngressItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.events).poll_next(cx) {
            Poll::Ready(None) => {
                self.ingress.close();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for ScanIngressStream {
    fn drop(&mut self) {
        for token in self.tokens.drain(..) {
            self.connection.stop_receive(token);
        }
        self.events.close();
        self.leases.clear();
        self.ingress.uninstall();
    }
}

#[cfg(test)]
mod tests {
    use super::{acquire_scan_rules, ScanIngress, ScanIngressItem};
    use crate::match_cleanup::{MatchFailure, MatchRegistry};
    use dbus::message::MatchRule;
    use dbus::Message;
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn signal() -> Message {
        Message::new_signal("/org/bluez/hci0", "org.bluez.Adapter1", "Changed")
            .expect("valid test signal")
    }

    #[tokio::test]
    async fn partial_rule_acquisition_releases_the_accepted_prefix() {
        let adds = Arc::new(AtomicUsize::new(0));
        let removes = Arc::new(AtomicUsize::new(0));
        let adds_for_op = adds.clone();
        let removes_for_op = removes.clone();
        let registry = MatchRegistry::new(
            Arc::new(move |_| {
                let attempt = adds_for_op.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if attempt == 0 {
                        Ok(())
                    } else {
                        Err(MatchFailure::new(
                            "org.bluez.Error.Failed",
                            "second rule refused",
                        ))
                    }
                })
            }),
            Arc::new(move |_| {
                removes_for_op.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }),
        );
        let rules = [
            MatchRule::new_signal("org.bluez.Adapter1", "Changed").static_clone(),
            MatchRule::new_signal("org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
                .static_clone(),
        ];
        assert!(acquire_scan_rules(rules, &registry, 91).await.is_err());
        registry.drain(91).await.unwrap();
        assert_eq!(adds.load(Ordering::SeqCst), 2);
        assert_eq!(removes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retire_then_fence_keeps_the_accepted_prefix_in_its_epoch() {
        let ingress = ScanIngress::new();
        let (sender, mut events) = futures::channel::mpsc::unbounded();
        ingress.install(sender).unwrap();
        ingress.arm(17).unwrap();
        assert!(ingress.accept(signal()));
        ingress.retire(17).unwrap();
        // A delayed callback cannot inherit a later epoch after retirement.
        assert!(ingress.accept(signal()));

        let fence_ingress = ingress.clone();
        let fence = tokio::spawn(async move { fence_ingress.fence(17).await });
        tokio::task::yield_now().await;

        assert!(matches!(
            events.next().await,
            Some(ScanIngressItem::Message {
                epoch: Some(17),
                ..
            })
        ));
        assert!(matches!(
            events.next().await,
            Some(ScanIngressItem::Message { epoch: None, .. })
        ));
        let Some(ScanIngressItem::Fence { epoch }) = events.next().await else {
            panic!("missing scan-ingress fence marker");
        };
        assert_eq!(epoch, 17);
        ingress.settle_fence(epoch);
        assert!(tokio::time::timeout(Duration::from_secs(1), fence)
            .await
            .expect("fence notification must not be lost")
            .unwrap()
            .is_ok());
        assert!(
            ingress.fence(17).await.is_ok(),
            "completed stop fence is retryable"
        );
        ingress.arm(18).unwrap();
    }

    #[test]
    fn stale_epoch_cannot_retire_a_replacement() {
        let ingress = ScanIngress::new();
        ingress.arm(17).unwrap();
        assert!(ingress.retire(18).is_err());
        ingress.retire(17).unwrap();
        assert!(ingress.arm(18).is_err(), "the old epoch is not fenced yet");
    }

    #[tokio::test]
    async fn authoritative_consumer_close_finishes_a_retiring_epoch_without_marker() {
        let ingress = ScanIngress::new();
        let (sender, _events) = futures::channel::mpsc::unbounded();
        ingress.install(sender).unwrap();
        ingress.arm(17).unwrap();
        assert!(ingress.accept(signal()));
        ingress.retire(17).unwrap();
        // This is called only by the owner after it observed the event stream
        // finish, so the accepted prefix is already drained.
        ingress.close();
        assert!(ingress.fence(17).await.is_ok());
        assert!(ingress.fence(17).await.is_ok(), "closed fence is retryable");
        assert!(
            ingress.arm(18).is_err(),
            "a closed event source cannot admit a dead scan"
        );
    }

    #[tokio::test]
    async fn unavailable_sender_does_not_poison_a_fence_retry() {
        let ingress = ScanIngress::new();
        ingress.arm(17).unwrap();
        ingress.retire(17).unwrap();
        assert!(ingress.fence(17).await.is_err());

        let (sender, mut events) = futures::channel::mpsc::unbounded();
        ingress.install(sender).unwrap();
        let retry_ingress = ingress.clone();
        let retry = tokio::spawn(async move { retry_ingress.fence(17).await });
        tokio::task::yield_now().await;
        let Some(ScanIngressItem::Fence { epoch }) = events.next().await else {
            panic!("retry did not enqueue a fence marker");
        };
        ingress.settle_fence(epoch);
        assert!(retry.await.unwrap().is_ok());
    }
}

impl MessageStream {
    pub async fn new(
        rule: MatchRule<'static>,
        connection: Arc<SyncConnection>,
        registry: &Arc<MatchRegistry>,
        scope: u64,
    ) -> Result<Self, MatchFailure> {
        let lease = registry.acquire(rule.match_str(), scope).await?;
        let (sender, events) = futures::channel::mpsc::unbounded();
        let token = connection.start_receive(
            rule,
            Box::new(move |message, _| sender.unbounded_send(message).is_ok()),
        );
        Ok(Self {
            token,
            events,
            connection,
            lease: Some(lease),
        })
    }
}

impl Stream for MessageStream {
    type Item = Message;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.events).poll_next(cx)
    }
}

impl Drop for MessageStream {
    fn drop(&mut self) {
        // Retire the local callback before receiver destruction can retire it
        // itself. Dispatch may temporarily own its token; a closed receiver
        // then makes the in-flight callback retire on its own.
        self.connection.stop_receive(self.token);
        self.events.close();
        // Even a missing local token leaves server cleanup owned by this lease.
        drop(self.lease.take());
    }
}
