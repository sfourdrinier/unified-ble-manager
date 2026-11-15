// vendor/btleplug/src/ubm_reclaim_authority.rs
//! Bounded source-owned reclaim authority and exact permit receipts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

const WINDOW: u64 = 256;
const MAX_AUTHORITIES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphKey {
    pub authority_id: u64,
    pub peer_id: String,
    pub connection_generation: String,
    pub graph: (String, u64, u64),
    pub permit_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphTarget {
    pub peer_id: String,
    pub connection_generation: String,
    pub graph: (String, u64, u64),
}

impl GraphKey {
    fn matches_target(&self, target: &GraphTarget) -> bool {
        self.peer_id == target.peer_id && self.connection_generation == target.connection_generation && self.graph == target.graph
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitTokenStatus { Pending, NotCreated, Issued(u64) }

struct PermitToken { target: GraphTarget, status: PermitTokenStatus, binding: Option<crate::ubm_reclaim_graph_binding::GraphBinding>, forgotten: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementAck { Acknowledged, AlreadyRetiredNoEffect }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenStatus {
    Pending,
    NotCreated,
    Opened(u64),
    Closed(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenReceipt {
    pub receipt_id: u64,
    pub status: OpenStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitStatus {
    Acquired,
    Busy,
    Stale,
    Invalidated,
    Committed,
    Released,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityError {
    Unknown,
    Conflict,
    Capacity,
    Pending,
    Closed,
    StillOwned,
}

#[derive(Clone)]
struct PermitReceipt {
    key: GraphKey,
    status: PermitStatus,
    binding: Option<crate::ubm_reclaim_graph_binding::GraphBinding>,
}
struct RetiredPermit {
    target: GraphTarget,
    binding: Option<crate::ubm_reclaim_graph_binding::GraphBinding>,
    acknowledged: bool,
}

struct Authority {
    open_receipt_id: u64,
    closed: bool,
    frontier: u64,
    retired: BTreeMap<u64, RetiredPermit>,
    permits: BTreeMap<u64, PermitReceipt>,
    tokens: HashMap<u64, PermitToken>,
    issued: BTreeMap<u64, u64>,
    next_permit: Option<u64>,
}

/// Callers serialize this registry with the corresponding native source owner.
/// `reserve_open` runs before queue submission; `resolve_open` is called once
/// the source itself accepts or refuses the open.
pub struct ReclaimAuthorities {
    by_token: HashMap<u64, u64>,
    opens: BTreeMap<u64, (u64, OpenStatus)>,
    next_open: Option<u64>,
    open_frontier: u64,
    retired_opens: BTreeSet<u64>,
    forgotten_opens: BTreeSet<u64>,
    authorities: BTreeMap<u64, Authority>,
    next_authority: Option<u64>,
    authority_frontier: u64,
    retired_authorities: BTreeSet<u64>,
}

/// Exact source-owned authority registry retained across adapter clones.
/// An unknown host token is never evidence that an asynchronous submission
/// could not still arrive; the host must fence that submission separately.
#[derive(Clone)]
pub struct ReclaimAuthorityHandle(Arc<Mutex<ReclaimAuthorities>>);
impl ReclaimAuthorityHandle {
    pub(crate) fn from_owner(owner: Arc<Mutex<ReclaimAuthorities>>) -> Self { Self(owner) }
    pub fn reserve_open(&self, token: u64) -> Result<OpenReceipt, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").reserve_open(token) }
    pub fn open_token_status(&self, token: u64) -> Result<OpenReceipt, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").open_token_status(token) }
    pub fn open_status(&self, token: u64, receipt: u64) -> Result<OpenStatus, AuthorityError> { let owner = self.0.lock().expect("reclaim authority lock poisoned"); let status = owner.open_token_status(token)?; if status.receipt_id != receipt { return Err(AuthorityError::Conflict); } Ok(status.status) }
    pub fn resolve_open(&self, token: u64, receipt: u64, create: bool) -> Result<OpenStatus, AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); if owner.open_token_status(token)?.receipt_id != receipt { return Err(AuthorityError::Conflict); } owner.resolve_open(receipt, create) }
    pub fn forget_not_created(&self, token: u64, receipt: u64) -> Result<(), AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); if owner.open_token_status(token)?.receipt_id != receipt { return Err(AuthorityError::Conflict); } owner.forget_not_created(receipt) }
    pub fn close(&self, token: u64, authority: u64) -> Result<(), AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); if !matches!(owner.open_token_status(token)?.status, OpenStatus::Opened(id) | OpenStatus::Closed(id) if id == authority) { return Err(AuthorityError::Conflict); } owner.close(authority) }
    pub fn forget_closed(&self, token: u64, authority: u64) -> Result<(), AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); let receipt = owner.open_token_status(token)?; if receipt.status != OpenStatus::Closed(authority) { return Err(AuthorityError::Conflict); } owner.forget_closed(authority) }
    pub fn ack_open_retirement(&self, token: u64, receipt: u64) -> Result<RetirementAck, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").ack_open_retirement(token, receipt) }
    pub fn reserve_permit_token_bound(&self, authority: u64, token: u64, target: GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<PermitTokenStatus, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").reserve_permit_token_bound(authority, token, target, binding) }
    pub fn permit_token_status_bound(&self, authority: u64, token: u64, target: &GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<PermitTokenStatus, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").permit_token_status_bound(authority, token, target, binding) }
    pub fn resolve_permit_token_bound(&self, authority: u64, token: u64, target: &GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding, issue: bool) -> Result<PermitTokenStatus, AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); owner.permit_token_status_bound(authority, token, target, binding)?; owner.resolve_permit_token(authority, token, target, issue) }
    pub fn forget_not_created_permit_token_bound(&self, authority: u64, token: u64, target: &GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<(), AuthorityError> { let mut owner = self.0.lock().expect("reclaim authority lock poisoned"); owner.permit_token_status_bound(authority, token, target, binding)?; owner.forget_not_created_permit_token(authority, token, target) }
    pub fn ack_permit_retirement(&self, key: &GraphKey, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<RetirementAck, AuthorityError> { self.0.lock().expect("reclaim authority lock poisoned").ack_permit_retirement(key, Some(binding)) }
    /// The host calls this only after receiving the authenticated forget and
    /// fencing all submissions. Unknown never claims historical ownership.
    pub fn ack_not_created_permit_token_bound(&self, authority: u64, token: u64, target: &GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<RetirementAck, AuthorityError> {
        let mut owner = self.0.lock().expect("reclaim authority lock poisoned");
        if !owner.authorities.contains_key(&authority) { return Err(AuthorityError::Unknown); }
        match owner.permit_token_status_bound(authority, token, target, binding) {
            Err(AuthorityError::Unknown) => Err(AuthorityError::Unknown),
            Err(error) => Err(error),
            Ok(_) => {
                let entry = owner.authorities.get_mut(&authority).and_then(|value| value.tokens.get(&token)).ok_or(AuthorityError::Unknown)?;
                if !entry.forgotten { return Err(AuthorityError::StillOwned); }
                owner.authorities.get_mut(&authority).ok_or(AuthorityError::Unknown)?.tokens.remove(&token);
                Ok(RetirementAck::Acknowledged)
            }
        }
    }
}

impl Default for ReclaimAuthorities {
    fn default() -> Self {
        Self { by_token: HashMap::new(), opens: BTreeMap::new(), next_open: Some(1), open_frontier: 0, retired_opens: BTreeSet::new(), forgotten_opens: BTreeSet::new(), authorities: BTreeMap::new(), next_authority: Some(1), authority_frontier: 0, retired_authorities: BTreeSet::new() }
    }
}

impl ReclaimAuthorities {
    pub fn open_token_status(&self, token: u64) -> Result<OpenReceipt, AuthorityError> {
        let receipt_id = *self.by_token.get(&token).ok_or(AuthorityError::Unknown)?;
        Ok(OpenReceipt { receipt_id, status: self.open_status(receipt_id)? })
    }
    pub fn reserve_open(&mut self, token: u64) -> Result<OpenReceipt, AuthorityError> {
        if let Some(id) = self.by_token.get(&token) {
            return Ok(OpenReceipt { receipt_id: *id, status: self.open_status(*id)? });
        }
        if token == 0 { return Err(AuthorityError::Conflict); }
        let id = self.next_open.ok_or(AuthorityError::Capacity)?;
        if self.opens.len() >= MAX_AUTHORITIES || id.saturating_sub(self.open_frontier) > WINDOW { return Err(AuthorityError::Capacity); }
        self.next_open = id.checked_add(1);
        self.by_token.insert(token, id);
        self.opens.insert(id, (token, OpenStatus::Pending));
        Ok(OpenReceipt { receipt_id: id, status: OpenStatus::Pending })
    }

    pub fn open_status(&self, receipt_id: u64) -> Result<OpenStatus, AuthorityError> {
        self.opens.get(&receipt_id).map(|(_, status)| *status).ok_or(AuthorityError::Unknown)
    }

    pub fn resolve_open(&mut self, receipt_id: u64, create: bool) -> Result<OpenStatus, AuthorityError> {
        let status = self.open_status(receipt_id)?;
        if status != OpenStatus::Pending { return Ok(status); }
        let next = self.next_authority.filter(|id| id.saturating_sub(self.authority_frontier) <= WINDOW);
        if !create || next.is_none() {
            self.opens.get_mut(&receipt_id).ok_or(AuthorityError::Unknown)?.1 = OpenStatus::NotCreated;
            return Ok(OpenStatus::NotCreated);
        }
        let id = next.ok_or(AuthorityError::Capacity)?;
        self.next_authority = id.checked_add(1);
        self.authorities.insert(id, Authority { open_receipt_id: receipt_id, closed: false, frontier: 0, retired: BTreeMap::new(), permits: BTreeMap::new(), tokens: HashMap::new(), issued: BTreeMap::new(), next_permit: Some(1) });
        self.opens.get_mut(&receipt_id).ok_or(AuthorityError::Unknown)?.1 = OpenStatus::Opened(id);
        Ok(OpenStatus::Opened(id))
    }

    /// Host must first fence every submitted open for this receipt.
    pub fn forget_not_created(&mut self, receipt_id: u64) -> Result<(), AuthorityError> {
        if receipt_id <= self.open_frontier || self.retired_opens.contains(&receipt_id) { return Err(AuthorityError::Unknown); }
        match self.open_status(receipt_id)? {
            OpenStatus::NotCreated => { self.forgotten_opens.insert(receipt_id); Ok(()) },
            OpenStatus::Pending => Err(AuthorityError::Pending),
            _ => Err(AuthorityError::StillOwned),
        }
    }

    fn retire_open(&mut self, receipt_id: u64) -> Result<(), AuthorityError> {
        let (token, _) = self.opens.remove(&receipt_id).ok_or(AuthorityError::Unknown)?;
        self.by_token.remove(&token);
        self.retired_opens.insert(receipt_id);
        while let Some(next) = self.open_frontier.checked_add(1) {
            if !self.retired_opens.remove(&next) { break; }
            self.open_frontier = next;
        }
        Ok(())
    }

    pub fn close(&mut self, id: u64) -> Result<(), AuthorityError> {
        let authority = self.authorities.get_mut(&id).ok_or(AuthorityError::Unknown)?;
        if !authority.permits.is_empty() || !authority.retired.is_empty() || !authority.tokens.is_empty() { return Err(AuthorityError::StillOwned); }
        authority.closed = true;
        self.opens.get_mut(&authority.open_receipt_id).ok_or(AuthorityError::Unknown)?.1 = OpenStatus::Closed(id);
        Ok(())
    }

    pub fn forget_closed(&mut self, id: u64) -> Result<(), AuthorityError> {
        if id <= self.authority_frontier || self.retired_authorities.contains(&id) { return Err(AuthorityError::Unknown); }
        let authority = self.authorities.get(&id).ok_or(AuthorityError::Unknown)?;
        if !authority.closed { return Err(AuthorityError::StillOwned); }
        let receipt_id = authority.open_receipt_id;
        self.forgotten_opens.insert(receipt_id);
        Ok(())
    }

    fn ack_open_retirement(&mut self, token: u64, receipt_id: u64) -> Result<RetirementAck, AuthorityError> {
        if receipt_id <= self.open_frontier || self.retired_opens.contains(&receipt_id) { return Ok(RetirementAck::AlreadyRetiredNoEffect); }
        let (expected, status) = *self.opens.get(&receipt_id).ok_or(AuthorityError::Unknown)?;
        if token != expected { return Err(AuthorityError::Conflict); }
        if !self.forgotten_opens.contains(&receipt_id) { return Err(AuthorityError::StillOwned); }
        match status {
            OpenStatus::NotCreated => {},
            OpenStatus::Closed(id) => {
                let authority = self.authorities.get(&id).ok_or(AuthorityError::Unknown)?;
                if !authority.closed || !authority.permits.is_empty() || !authority.retired.is_empty() || !authority.tokens.is_empty() { return Err(AuthorityError::StillOwned); }
                self.authorities.remove(&id);
                self.retired_authorities.insert(id);
                while let Some(next) = self.authority_frontier.checked_add(1) {
                    if !self.retired_authorities.remove(&next) { break; }
                    self.authority_frontier = next;
                }
            }
            _ => return Err(AuthorityError::StillOwned),
        }
        self.forgotten_opens.remove(&receipt_id);
        self.retire_open(receipt_id)?;
        Ok(RetirementAck::Acknowledged)
    }

    /// Reserve the host token before its source submission can be queued.
    pub fn reserve_permit_token(&mut self, authority_id: u64, token: u64, target: GraphTarget) -> Result<PermitTokenStatus, AuthorityError> {
        let authority = self.authorities.get_mut(&authority_id).ok_or(AuthorityError::Unknown)?;
        if authority.closed { return Err(AuthorityError::Closed); }
        if let Some(existing) = authority.tokens.get(&token) {
            return if existing.target == target { Ok(existing.status) } else { Err(AuthorityError::Conflict) };
        }
        if token == 0 { return Err(AuthorityError::Conflict); }
        if authority.tokens.len() >= WINDOW as usize { return Err(AuthorityError::Capacity); }
        if target.peer_id.len().saturating_add(target.connection_generation.len()).saturating_add(target.graph.0.len()) > 524_288 { return Err(AuthorityError::Capacity); }
        authority.tokens.insert(token, PermitToken { target, status: PermitTokenStatus::Pending, binding: None, forgotten: false });
        Ok(PermitTokenStatus::Pending)
    }

    pub fn reserve_permit_token_bound(&mut self, authority_id: u64, token: u64, target: GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<PermitTokenStatus, AuthorityError> {
        if let Some(entry) = self.authorities.get(&authority_id).and_then(|authority| authority.tokens.get(&token)) {
            if entry.target != target || entry.binding.as_ref().is_none_or(|existing| !existing.same_identity(binding)) { return Err(AuthorityError::Conflict); }
            return Ok(entry.status);
        }
        let status = self.reserve_permit_token(authority_id, token, target)?;
        let entry = self.authorities.get_mut(&authority_id).and_then(|authority| authority.tokens.get_mut(&token)).ok_or(AuthorityError::Unknown)?;
        entry.binding = Some(binding.clone());
        Ok(status)
    }

    pub fn permit_token_status(&self, authority_id: u64, token: u64, target: &GraphTarget) -> Result<PermitTokenStatus, AuthorityError> {
        let authority = self.authorities.get(&authority_id).ok_or(AuthorityError::Unknown)?;
        let entry = authority.tokens.get(&token).ok_or(AuthorityError::Unknown)?;
        if entry.target != *target { return Err(AuthorityError::Conflict); }
        Ok(entry.status)
    }
    pub fn permit_token_status_bound(&self, authority_id: u64, token: u64, target: &GraphTarget, binding: &crate::ubm_reclaim_graph_binding::GraphBinding) -> Result<PermitTokenStatus, AuthorityError> {
        let status = self.permit_token_status(authority_id, token, target)?;
        let entry = self.authorities.get(&authority_id).and_then(|authority| authority.tokens.get(&token)).ok_or(AuthorityError::Unknown)?;
        if entry.binding.as_ref().is_none_or(|existing| !existing.same_identity(binding)) { return Err(AuthorityError::Conflict); }
        Ok(status)
    }

    /// Resolve only from the source owner; retries replay the issued ID.
    pub fn resolve_permit_token(&mut self, authority_id: u64, token: u64, target: &GraphTarget, issue: bool) -> Result<PermitTokenStatus, AuthorityError> {
        let status = self.permit_token_status(authority_id, token, target)?;
        if status != PermitTokenStatus::Pending { return Ok(status); }
        let authority = self.authorities.get_mut(&authority_id).ok_or(AuthorityError::Unknown)?;
        if authority.closed { return Err(AuthorityError::Closed); }
        let next = authority.next_permit.filter(|id| id.saturating_sub(authority.frontier) <= WINDOW);
        let status = match (issue, next) {
            (true, Some(id)) => {
                authority.next_permit = id.checked_add(1);
                authority.issued.insert(id, token);
                PermitTokenStatus::Issued(id)
            }
            _ => PermitTokenStatus::NotCreated,
        };
        authority.tokens.get_mut(&token).ok_or(AuthorityError::Unknown)?.status = status;
        Ok(status)
    }

    /// The host must first fence all submissions for this token.
    pub fn forget_not_created_permit_token(&mut self, authority_id: u64, token: u64, target: &GraphTarget) -> Result<(), AuthorityError> {
        let authority = self.authorities.get_mut(&authority_id).ok_or(AuthorityError::Unknown)?;
        let entry = authority.tokens.get_mut(&token).ok_or(AuthorityError::Unknown)?;
        if entry.target != *target { return Err(AuthorityError::Conflict); }
        match entry.status {
            PermitTokenStatus::NotCreated => { entry.forgotten = true; Ok(()) }
            PermitTokenStatus::Pending => Err(AuthorityError::Pending),
            PermitTokenStatus::Issued(_) => Err(AuthorityError::StillOwned),
        }
    }

    /// `valid` and `busy` must be established under the same graph owner lock.
    pub fn acquire(&mut self, key: GraphKey, valid: bool, busy: bool) -> Result<PermitStatus, AuthorityError> {
        let authority = self.authorities.get_mut(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        if authority.closed { return Err(AuthorityError::Closed); }
        let token = *authority.issued.get(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        let reserved = authority.tokens.get(&token).ok_or(AuthorityError::Unknown)?;
        if !key.matches_target(&reserved.target) { return Err(AuthorityError::Conflict); }
        if let Some(receipt) = authority.permits.get(&key.permit_id) {
            return if receipt.key == key { Ok(receipt.status) } else { Err(AuthorityError::Conflict) };
        }
        if key.permit_id <= authority.frontier || authority.retired.contains_key(&key.permit_id) || key.permit_id.saturating_sub(authority.frontier) > WINDOW { return Err(AuthorityError::Conflict); }
        let status = if busy { PermitStatus::Busy } else if !valid { PermitStatus::Stale } else { PermitStatus::Acquired };
        authority.permits.insert(key.permit_id, PermitReceipt { key, status, binding: None });
        Ok(status)
    }

    /// Authenticate replay against the original opaque physical-source
    /// binding, including after that graph has retired.
    pub fn acquire_bound(&mut self, key: GraphKey, binding: &crate::ubm_reclaim_graph_binding::GraphBinding, valid: bool, busy: bool) -> Result<PermitStatus, AuthorityError> {
        let authority = self.authorities.get(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        let token = authority.issued.get(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        let reserved = authority.tokens.get(token).ok_or(AuthorityError::Unknown)?;
        if reserved.binding.as_ref().is_none_or(|existing| !existing.same_identity(binding)) { return Err(AuthorityError::Conflict); }
        if let Some(receipt) = self.authorities.get(&key.authority_id).and_then(|authority| authority.permits.get(&key.permit_id)) {
            if receipt.binding.as_ref().is_none_or(|existing| !existing.same_identity(binding)) { return Err(AuthorityError::Conflict); }
        }
        let status = self.acquire(key.clone(), valid, busy)?;
        let receipt = self.receipt_mut(&key)?;
        if receipt.binding.is_none() { receipt.binding = Some(binding.clone()); }
        Ok(status)
    }

    pub fn commit(&mut self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let receipt = self.receipt_mut(key)?;
        match receipt.status {
            PermitStatus::Acquired => { receipt.status = PermitStatus::Committed; Ok(receipt.status) }
            PermitStatus::Committed | PermitStatus::Invalidated => Ok(receipt.status),
            _ => Err(AuthorityError::Conflict),
        }
    }

    pub fn permit_status(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let authority = self.authorities.get(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        let receipt = authority.permits.get(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        if receipt.key != *key { return Err(AuthorityError::Conflict); }
        Ok(receipt.status)
    }
    pub fn authenticate_permit_owner(&self, key: &GraphKey, owner: &crate::ubm_reclaim_graph_binding::GraphBindingOwner) -> Result<(), AuthorityError> {
        let authority = self.authorities.get(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        if let Some(retired) = authority.retired.get(&key.permit_id) {
            if !key.matches_target(&retired.target) || retired.binding.as_ref().is_none_or(|binding| !owner.owns(binding)) { return Err(AuthorityError::Conflict); }
            return Ok(());
        }
        if let Some(receipt) = authority.permits.get(&key.permit_id) {
            if receipt.key != *key || receipt.binding.as_ref().is_none_or(|binding| !owner.owns(binding)) { return Err(AuthorityError::Conflict); }
            return Ok(());
        }
        let token = authority.issued.get(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        let entry = authority.tokens.get(token).ok_or(AuthorityError::Unknown)?;
        if !key.matches_target(&entry.target) || entry.binding.as_ref().is_none_or(|binding| !owner.owns(binding)) { return Err(AuthorityError::Conflict); }
        Ok(())
    }

    pub fn invalidate_before_commit(&mut self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let receipt = self.receipt_mut(key)?;
        if receipt.status == PermitStatus::Acquired { receipt.status = PermitStatus::Invalidated; }
        Ok(receipt.status)
    }

    pub fn release(&mut self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let receipt = self.receipt_mut(key)?;
        match receipt.status {
            PermitStatus::Acquired | PermitStatus::Committed | PermitStatus::Invalidated => { receipt.status = PermitStatus::Released; Ok(receipt.status) }
            PermitStatus::Released => Ok(PermitStatus::Released),
            _ => Err(AuthorityError::Conflict),
        }
    }

    pub fn forget_permit(&mut self, key: &GraphKey) -> Result<(), AuthorityError> {
        let authority = self.authorities.get_mut(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        if key.permit_id <= authority.frontier { return Err(AuthorityError::Unknown); }
        if let Some(retired) = authority.retired.get(&key.permit_id) {
            return if key.matches_target(&retired.target) { Ok(()) } else { Err(AuthorityError::Conflict) };
        }
        let token = *authority.issued.get(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        let reserved = authority.tokens.get(&token).ok_or(AuthorityError::Unknown)?;
        if !key.matches_target(&reserved.target) { return Err(AuthorityError::Conflict); }
        if let Some(receipt) = authority.permits.get(&key.permit_id) {
            if receipt.key != *key { return Err(AuthorityError::Conflict); }
            if matches!(receipt.status, PermitStatus::Acquired | PermitStatus::Committed | PermitStatus::Invalidated) { return Err(AuthorityError::StillOwned); }
        }
        let target = reserved.target.clone();
        let binding = reserved.binding.clone();
        authority.permits.remove(&key.permit_id);
        authority.issued.remove(&key.permit_id);
        authority.tokens.remove(&token);
        authority.retired.insert(key.permit_id, RetiredPermit { target, binding, acknowledged: false });
        Ok(())
    }

    /// After an authenticated forget response, the host no longer needs its
    /// tombstone. A retry below the contiguous frontier is a no-effect answer,
    /// not an assertion that an arbitrary binding owned that old receipt.
    fn ack_permit_retirement(&mut self, key: &GraphKey, binding: Option<&crate::ubm_reclaim_graph_binding::GraphBinding>) -> Result<RetirementAck, AuthorityError> {
        let authority = self.authorities.get_mut(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        if key.permit_id <= authority.frontier { return Ok(RetirementAck::AlreadyRetiredNoEffect); }
        let retired = authority.retired.get_mut(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        if !key.matches_target(&retired.target) || match (&retired.binding, binding) {
            (Some(expected), Some(actual)) => !expected.same_identity(actual),
            (None, None) => false,
            _ => true,
        } { return Err(AuthorityError::Conflict); }
        retired.acknowledged = true;
        while let Some(next) = authority.frontier.checked_add(1) {
            if !authority.retired.get(&next).is_some_and(|entry| entry.acknowledged) { break; }
            authority.retired.remove(&next);
            authority.frontier = next;
        }
        Ok(RetirementAck::Acknowledged)
    }

    fn receipt_mut(&mut self, key: &GraphKey) -> Result<&mut PermitReceipt, AuthorityError> {
        let authority = self.authorities.get_mut(&key.authority_id).ok_or(AuthorityError::Unknown)?;
        if authority.closed { return Err(AuthorityError::Closed); }
        let receipt = authority.permits.get_mut(&key.permit_id).ok_or(AuthorityError::Unknown)?;
        if receipt.key != *key { return Err(AuthorityError::Conflict); }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(authority_id: u64, permit_id: u64) -> GraphKey {
        GraphKey { authority_id, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("owner".into(), 1, 2), permit_id }
    }
    fn target(key: &GraphKey) -> GraphTarget {
        GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: key.graph.clone() }
    }
    fn issue(owner: &mut ReclaimAuthorities, key: &GraphKey) {
        let target = target(key);
        assert_eq!(owner.reserve_permit_token(key.authority_id, key.permit_id, target.clone()), Ok(PermitTokenStatus::Pending));
        assert_eq!(owner.resolve_permit_token(key.authority_id, key.permit_id, &target, true), Ok(PermitTokenStatus::Issued(key.permit_id)));
        assert_eq!(owner.resolve_permit_token(key.authority_id, key.permit_id, &target, true), Ok(PermitTokenStatus::Issued(key.permit_id)));
    }

    #[test]
    fn open_is_pending_until_source_resolves_and_retry_keeps_same_id() {
        let mut owner = ReclaimAuthorities::default();
        let receipt = OpenReceipt { receipt_id: 1, status: OpenStatus::Pending };
        assert_eq!(owner.reserve_open(17), Ok(receipt));
        assert_eq!(owner.open_status(1), Ok(OpenStatus::Pending));
        assert_eq!(owner.forget_not_created(1), Err(AuthorityError::Pending));
        assert_eq!(owner.reserve_open(17), Ok(receipt));
        assert_eq!(owner.resolve_open(1, true), Ok(OpenStatus::Opened(1)));
        assert_eq!(owner.resolve_open(1, true), Ok(OpenStatus::Opened(1)));
        assert_eq!(owner.reserve_open(18), Ok(OpenReceipt { receipt_id: 2, status: OpenStatus::Pending }));
        assert_eq!(owner.resolve_open(2, false), Ok(OpenStatus::NotCreated));
        owner.forget_not_created(2).unwrap();
        owner.forget_not_created(2).unwrap();
        assert_eq!(owner.ack_open_retirement(19, 2), Err(AuthorityError::Conflict));
        assert_eq!(owner.ack_open_retirement(18, 2), Ok(RetirementAck::Acknowledged));
        assert_eq!(owner.ack_open_retirement(18, 2), Ok(RetirementAck::AlreadyRetiredNoEffect));
        assert_eq!(owner.open_status(2), Err(AuthorityError::Unknown));
    }

    #[test]
    fn source_handle_replays_exact_open_token_across_clones() {
        let handle = ReclaimAuthorityHandle::from_owner(Arc::new(Mutex::new(ReclaimAuthorities::default())));
        let clone = handle.clone();
        assert_eq!(handle.open_token_status(17), Err(AuthorityError::Unknown));
        let receipt = handle.reserve_open(17).unwrap();
        assert_eq!(clone.open_token_status(17), Ok(receipt));
        assert_eq!(clone.resolve_open(17, receipt.receipt_id, true), Ok(OpenStatus::Opened(1)));
        assert_eq!(handle.reserve_open(17).unwrap().status, OpenStatus::Opened(1));
        handle.reserve_open(18).unwrap();
        assert_eq!(handle.open_status(18, receipt.receipt_id), Err(AuthorityError::Conflict));
        assert_eq!(handle.resolve_open(18, receipt.receipt_id, false), Err(AuthorityError::Conflict));
        assert_eq!(handle.close(18, 1), Err(AuthorityError::Conflict));
        handle.close(17, 1).unwrap();
        assert_eq!(handle.forget_closed(18, 1), Err(AuthorityError::Conflict));
        handle.forget_closed(17, 1).unwrap();
        handle.forget_closed(17, 1).unwrap();
        assert_eq!(handle.ack_open_retirement(18, receipt.receipt_id), Err(AuthorityError::Conflict));
        assert_eq!(handle.ack_open_retirement(17, receipt.receipt_id), Ok(RetirementAck::Acknowledged));
        assert_eq!(handle.ack_open_retirement(17, receipt.receipt_id), Ok(RetirementAck::AlreadyRetiredNoEffect));
    }

    #[test]
    fn permit_token_transitions_require_exact_captured_source_binding() {
        use crate::ubm_reclaim_graph_binding::GraphBindingOwner;
        let handle = ReclaimAuthorityHandle::from_owner(Arc::new(Mutex::new(ReclaimAuthorities::default())));
        handle.reserve_open(1).unwrap(); handle.resolve_open(1, 1, true).unwrap();
        let graph = ("native".to_owned(), 1, 2);
        let mut first = GraphBindingOwner::default(); first.attach(); first.publish(graph.clone());
        let mut foreign = GraphBindingOwner::default(); foreign.attach(); foreign.publish(graph.clone());
        let binding = first.capture("peer".into(), "connection".into(), &graph).unwrap();
        let other = foreign.capture("peer".into(), "connection".into(), &graph).unwrap();
        let target = GraphTarget { peer_id: "peer".into(), connection_generation: "connection".into(), graph };
        assert_eq!(handle.reserve_permit_token_bound(1, 7, target.clone(), &binding), Ok(PermitTokenStatus::Pending));
        assert_eq!(handle.reserve_permit_token_bound(1, 7, target.clone(), &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.permit_token_status_bound(1, 7, &target, &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.resolve_permit_token_bound(1, 7, &target, &other, false), Err(AuthorityError::Conflict));
        assert_eq!(handle.forget_not_created_permit_token_bound(1, 7, &target, &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.resolve_permit_token_bound(1, 7, &target, &binding, false), Ok(PermitTokenStatus::NotCreated));
        assert_eq!(handle.forget_not_created_permit_token_bound(1, 7, &target, &other), Err(AuthorityError::Conflict));
        handle.forget_not_created_permit_token_bound(1, 7, &target, &binding).unwrap();
        handle.forget_not_created_permit_token_bound(1, 7, &target, &binding).unwrap();
        // Before ACK dispatch, exact status and the bounded tombstone remain.
        assert_eq!(handle.permit_token_status_bound(1, 7, &target, &binding), Ok(PermitTokenStatus::NotCreated));
        assert_eq!(handle.forget_not_created_permit_token_bound(1, 7, &target, &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.ack_not_created_permit_token_bound(1, 7, &target, &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.ack_not_created_permit_token_bound(1, 7, &target, &binding), Ok(RetirementAck::Acknowledged));
        assert_eq!(handle.ack_not_created_permit_token_bound(1, 7, &target, &binding), Err(AuthorityError::Unknown));
        let restarted = ReclaimAuthorityHandle::from_owner(Arc::new(Mutex::new(ReclaimAuthorities::default())));
        assert_eq!(restarted.ack_not_created_permit_token_bound(1, 7, &target, &binding), Err(AuthorityError::Unknown));
        assert_eq!(handle.reserve_permit_token_bound(1, 8, target.clone(), &binding), Ok(PermitTokenStatus::Pending));
        assert_eq!(handle.resolve_permit_token_bound(1, 8, &target, &binding, true), Ok(PermitTokenStatus::Issued(1)));
        let key = GraphKey { authority_id: 1, peer_id: target.peer_id.clone(), connection_generation: target.connection_generation.clone(), graph: target.graph.clone(), permit_id: 1 };
        let mut owner = handle.0.lock().unwrap();
        assert_eq!(owner.authenticate_permit_owner(&key, &foreign), Err(AuthorityError::Conflict));
        assert_eq!(owner.authenticate_permit_owner(&key, &first), Ok(()));
        assert_eq!(owner.acquire_bound(key.clone(), &other, true, false), Err(AuthorityError::Conflict));
        assert_eq!(owner.acquire_bound(key.clone(), &binding, true, false), Ok(PermitStatus::Acquired));
        first.disconnect();
        assert_eq!(owner.acquire_bound(key.clone(), &binding, false, true), Ok(PermitStatus::Acquired));
        owner.release(&key).unwrap();
        owner.forget_permit(&key).unwrap();
        assert_eq!(owner.authenticate_permit_owner(&key, &foreign), Err(AuthorityError::Conflict));
        assert_eq!(owner.authenticate_permit_owner(&key, &first), Ok(()));
        owner.forget_permit(&key).unwrap();
        assert_eq!(owner.close(1), Err(AuthorityError::StillOwned));
        drop(owner);
        assert_eq!(handle.ack_permit_retirement(&key, &other), Err(AuthorityError::Conflict));
        assert_eq!(handle.ack_permit_retirement(&key, &binding), Ok(RetirementAck::Acknowledged));
        assert_eq!(handle.ack_permit_retirement(&key, &binding), Ok(RetirementAck::AlreadyRetiredNoEffect));
        let mut future = key.clone(); future.permit_id = 2;
        assert_eq!(handle.ack_permit_retirement(&future, &binding), Err(AuthorityError::Unknown));
    }

    #[test]
    fn permits_preserve_exact_identity_invalidation_and_retirement() {
        let mut owner = ReclaimAuthorities::default();
        owner.reserve_open(9).unwrap();
        owner.resolve_open(1, true).unwrap();
        let first = key(1, 1);
        issue(&mut owner, &first);
        assert_eq!(owner.acquire(first.clone(), true, false), Ok(PermitStatus::Acquired));
        assert_eq!(owner.acquire(first.clone(), false, true), Ok(PermitStatus::Acquired));
        let mut conflict = first.clone(); conflict.graph.2 += 1;
        assert_eq!(owner.acquire(conflict, true, false), Err(AuthorityError::Conflict));
        assert_eq!(owner.invalidate_before_commit(&first), Ok(PermitStatus::Invalidated));
        assert_eq!(owner.commit(&first), Ok(PermitStatus::Invalidated));
        assert_eq!(owner.close(1), Err(AuthorityError::StillOwned));
        assert_eq!(owner.release(&first), Ok(PermitStatus::Released));
        assert_eq!(owner.release(&first), Ok(PermitStatus::Released));
        owner.forget_permit(&first).unwrap();
        owner.ack_permit_retirement(&first, None).unwrap();
        let second = key(1, 2);
        issue(&mut owner, &second);
        assert_eq!(owner.acquire(second.clone(), true, false), Ok(PermitStatus::Acquired));
        assert_eq!(owner.commit(&second), Ok(PermitStatus::Committed));
        assert_eq!(owner.invalidate_before_commit(&second), Ok(PermitStatus::Committed));
        owner.release(&second).unwrap();
        owner.forget_permit(&second).unwrap();
        owner.ack_permit_retirement(&second, None).unwrap();
        owner.close(1).unwrap();
        assert_eq!(owner.open_status(1), Ok(OpenStatus::Closed(1)));
        owner.forget_closed(1).unwrap();
        owner.forget_closed(1).unwrap();
        assert_eq!(owner.ack_open_retirement(10, 1), Err(AuthorityError::Conflict));
        assert_eq!(owner.ack_open_retirement(9, 1), Ok(RetirementAck::Acknowledged));
        assert_eq!(owner.ack_open_retirement(9, 1), Ok(RetirementAck::AlreadyRetiredNoEffect));
        assert_eq!(owner.ack_open_retirement(9, 3), Err(AuthorityError::Unknown));
    }

    #[test]
    fn permit_window_is_bounded_even_when_retired_out_of_order() {
        let mut owner = ReclaimAuthorities::default();
        owner.reserve_open(1).unwrap(); owner.resolve_open(1, true).unwrap();
        let far = key(1, WINDOW + 1);
        assert_eq!(owner.acquire(far, true, false), Err(AuthorityError::Unknown));
        let first = key(1, 1);
        issue(&mut owner, &first);
        let second = key(1, 2);
        issue(&mut owner, &second);
        assert_eq!(owner.acquire(second.clone(), false, false), Ok(PermitStatus::Stale));
        owner.forget_permit(&second).unwrap();
        owner.ack_permit_retirement(&second, None).unwrap();
        assert_eq!(owner.close(1), Err(AuthorityError::StillOwned));
        owner.acquire(first.clone(), false, false).unwrap();
        owner.forget_permit(&first).unwrap();
        owner.ack_permit_retirement(&first, None).unwrap();
        owner.close(1).unwrap();
    }

    #[test]
    fn pending_permit_token_cannot_be_assumed_absent_and_issued_no_effect_can_retire() {
        let mut owner = ReclaimAuthorities::default();
        owner.reserve_open(1).unwrap(); owner.resolve_open(1, true).unwrap();
        let first = key(1, 1); let target = target(&first);
        assert_eq!(owner.reserve_permit_token(1, 42, target.clone()), Ok(PermitTokenStatus::Pending));
        assert_eq!(owner.reserve_permit_token(1, 42, target.clone()), Ok(PermitTokenStatus::Pending));
        let mut foreign = target.clone(); foreign.graph.2 += 1;
        assert_eq!(owner.reserve_permit_token(1, 42, foreign), Err(AuthorityError::Conflict));
        assert_eq!(owner.forget_not_created_permit_token(1, 42, &target), Err(AuthorityError::Pending));
        assert_eq!(owner.resolve_permit_token(1, 42, &target, true), Ok(PermitTokenStatus::Issued(1)));
        assert_eq!(owner.permit_token_status(1, 42, &target), Ok(PermitTokenStatus::Issued(1)));
        owner.forget_permit(&first).unwrap();
        owner.forget_permit(&first).unwrap();
        owner.ack_permit_retirement(&first, None).unwrap();
        owner.close(1).unwrap();
    }
}
