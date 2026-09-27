//! Mobile adapter to the one shared native continuation state machine.
use crate::{MobileHost, MobileSession, wire};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
pub use ubm_desktop::continuation::NativeContinuation;
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, Result, envelope,
};

struct HostAdapter(Weak<crate::host::HostInner>);
struct SessionAdapter {
    host: Weak<crate::host::HostInner>,
    id: u64,
}

fn gone() -> Value {
    json!({"code":"lifecycle.destroyed","domain":"restoration","operation":"continuation","detail":"native host or session is gone"})
}

impl ContinuationHost for HostAdapter {
    fn canonical_peer(&self, peer: &str) -> String {
        let separators: Option<(u8, &[usize])> = match peer.len() {
            17 => Some((b':', &[2, 5, 8, 11, 14])),
            36 => Some((b'-', &[8, 13, 18, 23])),
            _ => None,
        };
        if separators.is_some_and(|(separator, positions)| {
            peer.bytes().enumerate().all(|(index, byte)| {
                if positions.contains(&index) {
                    byte == separator
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        }) {
            peer.to_ascii_uppercase()
        } else {
            peer.to_owned()
        }
    }

    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        let inner = self.0.upgrade().ok_or_else(gone)?;
        let _admission = inner
            .continuation_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.continuation_closed.load(Ordering::SeqCst) {
            return Err(gone());
        }
        let session = MobileHost {
            inner: Arc::clone(&inner),
        }
        .open_native_session("unified-ble-manager/continuation")
        .map_err(|error| Value::Object(wire::error_object(&error)))?;
        Ok(Arc::new(SessionAdapter {
            host: self.0.clone(),
            id: session.id(),
        }))
    }
}

impl SessionAdapter {
    fn resolve(&self) -> Result<MobileSession> {
        let inner = self.host.upgrade().ok_or_else(gone)?;
        MobileHost { inner }.session(self.id).ok_or_else(gone)
    }
}

impl ContinuationSession for SessionAdapter {
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            if matches!(
                op,
                "connection.connect" | "gatt.discover" | "gatt.subscribe"
            ) && self
                .host
                .upgrade()
                .is_none_or(|host| host.continuation_closed.load(Ordering::SeqCst))
            {
                return envelope(Err(gone()));
            }
            match self.resolve() {
                Ok(session) => session.call(op, args).await,
                Err(error) => envelope(Err(error)),
            }
        })
    }
    fn drain(&self, items: u32, bytes: u32) -> ContinuationFuture<'_> {
        Box::pin(async move {
            match self.resolve() {
                Ok(session) => session.drain(items, bytes),
                Err(error) => envelope(Err(error)),
            }
        })
    }
}

impl MobileHost {
    pub fn continuation_reserve_declaration(&self, declaration: &str) -> String {
        envelope(self.continuation().reserve_declaration(declaration))
    }
    pub fn continuation_commit_declaration(&self, token: &str) -> String {
        envelope(self.continuation().commit_declaration(token))
    }
    pub fn continuation_cancel_declaration(&self, token: &str) -> String {
        envelope(self.continuation().cancel_declaration(token))
    }
    pub fn continuation_seed_declaration(&self, declaration: &str) -> String {
        envelope(self.continuation().seed_declaration(declaration))
    }
    pub fn continuation(&self) -> NativeContinuation {
        self.inner
            .continuation
            .get_or_init(|| {
                NativeContinuation::new(Arc::new(HostAdapter(Arc::downgrade(&self.inner))))
            })
            .clone()
    }
    pub fn continuation_declaration_replacement_failure(
        &self,
        declaration: &str,
    ) -> Option<String> {
        self.continuation()
            .declaration_replacement_failure(declaration)
    }
    pub fn continuation_describe_backlog(&self, completion: crate::Completion) {
        let executor = self.continuation();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.describe_backlog().await));
        });
    }
    pub fn continuation_execute(
        &self,
        peer: &str,
        declaration: &str,
        completion: crate::Completion,
    ) {
        if self.inner.continuation_closed.load(Ordering::SeqCst) {
            completion(envelope(Err(gone())));
            return;
        }
        let executor = self.continuation();
        let peer = peer.to_owned();
        let declaration = declaration.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.execute(&peer, &declaration).await));
        });
    }
    pub fn continuation_prepare_claim(
        &self,
        items: u32,
        bytes: u32,
        completion: crate::Completion,
    ) {
        let executor = self.continuation();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.prepare_claim(items, bytes).await));
        });
    }
    pub fn continuation_acknowledge_claim(&self, token: &str, completion: crate::Completion) {
        let executor = self.continuation();
        let token = token.to_owned();
        self.inner.runtime.spawn(async move {
            completion(envelope(executor.acknowledge_claim(&token).await));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_peer_canonicalization_is_limited_to_uuid_and_mac() {
        let host = HostAdapter(Weak::new());
        for peer in ["aa:bb:cc:dd:ee:ff", "abcdef01-2345-6789-abcd-ef0123456789"] {
            assert_eq!(host.canonical_peer(peer), peer.to_ascii_uppercase());
        }
        for peer in [
            "hci1/dev_aa_bb_cc_dd_ee_ff",
            "opaque-Peer",
            "not-a-uuid",
            "gg:bb:cc:dd:ee:ff",
        ] {
            assert_eq!(host.canonical_peer(peer), peer);
        }
    }
}
