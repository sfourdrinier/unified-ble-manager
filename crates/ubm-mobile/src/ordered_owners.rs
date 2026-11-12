// crates/ubm-mobile/src/ordered_owners.rs
//! Bounded host-owned identities for ordered groups and bridge recovery.
use crate::foreign::lock;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_desktop::DesktopError;

const MAX_OWNERS: usize = 128;
const NONCE_BYTES: usize = 32;
const TOKEN_BYTES: usize = 64;
type Tag = Hmac<Sha256>;
struct Owner {
    peer: String,
    session: Option<u64>,
    origin: u64,
    completed: bool,
    context: Option<serde_json::Value>,
}
#[derive(Default)]
struct State {
    secret: Option<[u8; 32]>,
    owners: HashMap<u64, Owner>,
}
/// Holds exact ownership until source and physical cleanup are acknowledged.
#[derive(Default)]
pub(crate) struct OrderedOwners(Mutex<State>);
fn failure(code: BleErrorCode, detail: &'static str) -> DesktopError {
    DesktopError::new(code, BleErrorDomain::Core, "gatt.ordered.owner").with_detail(detail)
}
fn mac(secret: &[u8; 32], id: u64, peer: &str, nonce: &[u8]) -> Tag {
    let mut mac = Tag::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"ubm-mobile-ordered-reclaim-v1\0");
    mac.update(&id.to_be_bytes());
    mac.update(&(peer.len() as u64).to_be_bytes());
    mac.update(peer.as_bytes());
    mac.update(nonce);
    mac
}
fn decode(text: &str) -> Option<[u8; TOKEN_BYTES]> {
    if text.len() != TOKEN_BYTES * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut result = [0; TOKEN_BYTES];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        result[index] = nibble(chunk[0]) << 4 | nibble(chunk[1]);
    }
    Some(result)
}
fn valid(state: &State, id: u64, peer: &str, text: &str) -> bool {
    let (Some(secret), Some(token)) = (&state.secret, decode(text)) else {
        return false;
    };
    mac(secret, id, peer, &token[..NONCE_BYTES])
        .verify_slice(&token[NONCE_BYTES..])
        .is_ok()
}
impl OrderedOwners {
    pub(crate) fn reserve(
        &self,
        peer: &str,
        session: u64,
        reserve: impl FnOnce() -> Result<u64, DesktopError>,
    ) -> Result<(u64, String), DesktopError> {
        let mut nonce = [0; NONCE_BYTES];
        getrandom::fill(&mut nonce)
            .map_err(|_| failure(BleErrorCode::PlatformFailure, "secure entropy unavailable"))?;
        let mut state = lock(&self.0);
        if state.owners.len() >= MAX_OWNERS {
            return Err(failure(
                BleErrorCode::LifecycleInvalidState,
                "ordered owner capacity reached",
            ));
        }
        if state.secret.is_none() {
            let mut secret = [0; 32];
            getrandom::fill(&mut secret).map_err(|_| {
                failure(BleErrorCode::PlatformFailure, "secure entropy unavailable")
            })?;
            state.secret = Some(secret);
        }
        let id = reserve()?;
        if state.owners.contains_key(&id) {
            return Err(failure(
                BleErrorCode::LifecycleInvariantViolation,
                "ordered identity reused",
            ));
        }
        let mut token = [0; TOKEN_BYTES];
        token[..NONCE_BYTES].copy_from_slice(&nonce);
        let secret = state
            .secret
            .as_ref()
            .expect("secret initialized before reservation");
        token[NONCE_BYTES..]
            .copy_from_slice(&mac(secret, id, peer, &nonce).finalize().into_bytes());
        state.owners.insert(
            id,
            Owner {
                peer: peer.to_owned(),
                session: Some(session),
                origin: session,
                completed: false,
                context: None,
            },
        );
        let mut text = String::with_capacity(TOKEN_BYTES * 2);
        for byte in token {
            use std::fmt::Write;
            let _ = write!(text, "{byte:02x}");
        }
        Ok((id, text))
    }
    pub(crate) fn owns(&self, id: u64, peer: &str, session: u64) -> bool {
        lock(&self.0)
            .owners
            .get(&id)
            .is_some_and(|o| o.peer == peer && o.session == Some(session))
    }
    /// Serialize a synchronous mutation with detach, reclaim and completion.
    pub(crate) fn with_current<T>(
        &self,
        id: u64,
        peer: &str,
        session: u64,
        action: impl FnOnce() -> Result<T, DesktopError>,
    ) -> Result<T, DesktopError> {
        let state = lock(&self.0);
        if !state.owners.get(&id).is_some_and(|owner| {
            owner.peer == peer && owner.session == Some(session) && !owner.completed
        }) {
            return Err(failure(
                BleErrorCode::OwnershipDenied,
                "ordered owner is not current",
            ));
        }
        action()
    }
    pub(crate) fn groups(&self, session: u64) -> Vec<(u64, String)> {
        lock(&self.0)
            .owners
            .iter()
            .filter_map(|(id, o)| {
                (o.origin == session || o.session == Some(session)).then(|| (*id, o.peer.clone()))
            })
            .collect()
    }
    pub(crate) fn detach_group(&self, id: u64, peer: &str, session: u64) -> bool {
        let mut state = lock(&self.0);
        let Some(owner) = state.owners.get_mut(&id) else {
            return false;
        };
        if owner.peer != peer || owner.session != Some(session) {
            return false;
        }
        owner.session = None;
        true
    }
    pub(crate) fn retire_host_owned(&self, id: u64, peer: &str) -> bool {
        let mut state = lock(&self.0);
        if !state
            .owners
            .get(&id)
            .is_some_and(|owner| owner.peer == peer)
        {
            return false;
        }
        state.owners.remove(&id);
        true
    }
    pub(crate) fn detach(&self, session: u64) {
        for owner in lock(&self.0).owners.values_mut() {
            if owner.session == Some(session) {
                owner.session = None;
            }
        }
    }
    // The recorder's separate process-owned capability has already been authenticated.
    pub(crate) fn reclaim_recording(&self, id: u64, peer: &str, session: u64) -> bool {
        let mut state = lock(&self.0);
        let Some(owner) = state.owners.get_mut(&id) else {
            return false;
        };
        if owner.peer != peer || owner.session.is_some_and(|current| current != session) {
            return false;
        }
        owner.session = Some(session);
        true
    }
    pub(crate) fn reclaim(&self, id: u64, peer: &str, session: u64, capability: &str) -> bool {
        let mut state = lock(&self.0);
        if !valid(&state, id, peer, capability) {
            return false;
        }
        let Some(owner) = state.owners.get_mut(&id) else {
            return false;
        };
        if owner.peer != peer || owner.session.is_some_and(|current| current != session) {
            return false;
        }
        owner.session = Some(session);
        true
    }
    pub(crate) fn bind_context(
        &self,
        id: u64,
        peer: &str,
        session: u64,
        context: serde_json::Value,
    ) -> Result<(), DesktopError> {
        let mut state = lock(&self.0);
        let owner = state.owners.get_mut(&id).ok_or_else(|| {
            failure(
                BleErrorCode::OwnershipDenied,
                "ordered owner is unavailable",
            )
        })?;
        if owner.peer != peer || owner.session != Some(session) || owner.completed {
            return Err(failure(
                BleErrorCode::OwnershipDenied,
                "ordered owner is not current",
            ));
        }
        match &owner.context {
            Some(existing) if existing != &context => Err(failure(
                BleErrorCode::ArgumentInvalid,
                "ordered context differs from original",
            )),
            Some(_) => Ok(()),
            None => {
                owner.context = Some(context);
                Ok(())
            }
        }
    }
    pub(crate) fn context(
        &self,
        id: u64,
        peer: &str,
        session: u64,
    ) -> Option<Option<serde_json::Value>> {
        lock(&self.0)
            .owners
            .get(&id)
            .filter(|owner| owner.peer == peer && owner.session == Some(session))
            .map(|owner| owner.context.clone())
    }
    pub(crate) fn authenticate(&self, id: u64, peer: &str, capability: &str) -> bool {
        valid(&lock(&self.0), id, peer, capability)
    }
    pub(crate) fn absent(&self, id: u64) -> bool {
        !lock(&self.0).owners.contains_key(&id)
    }
    pub(crate) fn completed(&self, id: u64, peer: &str, session: u64) -> bool {
        lock(&self.0)
            .owners
            .get(&id)
            .is_some_and(|o| o.peer == peer && o.session == Some(session) && o.completed)
    }
    pub(crate) fn mark_completed(&self, id: u64, peer: &str, session: u64) -> bool {
        let mut s = lock(&self.0);
        let Some(o) = s.owners.get_mut(&id) else {
            return false;
        };
        if o.peer != peer || o.session != Some(session) {
            return false;
        }
        o.completed = true;
        true
    }
    pub(crate) fn remove(&self, id: u64, peer: &str, session: u64) -> bool {
        let mut s = lock(&self.0);
        if !s
            .owners
            .get(&id)
            .is_some_and(|o| o.peer == peer && o.session == Some(session))
        {
            return false;
        }
        s.owners.remove(&id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_is_host_and_group_bound_and_reclaim_requires_detach() {
        let owners = OrderedOwners::default();
        let (id, capability) = owners.reserve("peer", 1, || Ok(17)).unwrap();
        let context = serde_json::json!({"members":[{"id":"original"}]});
        owners.bind_context(id, "peer", 1, context.clone()).unwrap();
        owners.bind_context(id, "peer", 1, context.clone()).unwrap();
        assert!(
            owners
                .bind_context(id, "peer", 1, serde_json::json!({"members":[]}))
                .is_err()
        );
        assert!(!owners.reclaim(id, "peer", 2, &capability));
        owners.detach(1);
        assert!(!owners.reclaim(id, "other", 2, &capability));
        assert!(!owners.reclaim(id + 1, "peer", 2, &capability));
        let mut bad = capability.clone().into_bytes();
        bad[0] = if bad[0] == b'0' { b'1' } else { b'0' };
        assert!(!owners.reclaim(id, "peer", 2, std::str::from_utf8(&bad).unwrap()));
        let other = OrderedOwners::default();
        other.reserve("peer", 1, || Ok(17)).unwrap();
        assert!(!other.authenticate(id, "peer", &capability));
        assert!(owners.reclaim(id, "peer", 2, &capability));
        assert_eq!(owners.context(id, "peer", 2), Some(Some(context)));
        assert_eq!(owners.groups(1), vec![(id, "peer".to_owned())]);
        assert_eq!(owners.groups(2), vec![(id, "peer".to_owned())]);
        owners.detach(2);
        assert!(owners.reclaim(id, "peer", 3, &capability));
        assert!(owners.remove(id, "peer", 3));
        assert!(owners.authenticate(id, "peer", &capability));
        assert!(!owners.reclaim(id, "peer", 3, &capability));
    }
    #[test]
    fn capacity_is_checked_before_reservation_effect() {
        let owners = OrderedOwners::default();
        for id in 0..MAX_OWNERS {
            owners.reserve("peer", 1, || Ok(id as u64)).unwrap();
        }
        assert!(
            owners
                .reserve("peer", 1, || panic!("must not reserve"))
                .is_err()
        );
    }
}
