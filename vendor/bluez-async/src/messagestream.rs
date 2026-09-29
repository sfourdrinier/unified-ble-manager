use crate::match_cleanup::{MatchFailure, MatchLease, MatchRegistry};
use dbus::Message;
use dbus::channel::{MatchingReceiver, Token};
use dbus::message::MatchRule;
use dbus::nonblock::SyncConnection;
use futures::Stream;
use futures::channel::mpsc::UnboundedReceiver;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Local callback ownership is distinct from the connection's shared server
/// rule. A missing local token must never be mistaken for a removed bus rule.
pub struct MessageStream {
    token: Token,
    events: UnboundedReceiver<Message>,
    connection: Arc<SyncConnection>,
    lease: Option<MatchLease>,
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
