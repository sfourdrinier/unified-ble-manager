// vendor/btleplug/src/bluez/peer_source_fixture.rs
//! No-radio access to a real peer-source reclaim gate.

use super::*;

/// No-radio fixture using the real BlueZ source registry and graph gate.
#[cfg(feature = "reclaim-test-support")]
pub struct ReclaimSourceFixture {
    peer: Arc<PeerSource>,
    handle: crate::ubm_reclaim_authority::ReclaimAuthorityHandle,
    binding: GraphBinding,
}
#[cfg(feature = "reclaim-test-support")]
impl ReclaimSourceFixture {
    pub fn new(peer_id: &str, connection_generation: &str, graph: (String, u64, u64)) -> Self {
        let registry = PeerRegistry::default();
        let peer = registry.get_by_peer(peer_id).expect("fresh peer registry has capacity");
        let binding = {
            let mut state = peer.state.lock().expect("peer source lock poisoned");
            state.watching = true; state.connected = true; state.resolved = true;
            state.graph_revision = Some(state.revision);
            state.reclaim_binding.attach();
            state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(peer_id.to_owned(), connection_generation.to_owned(), &graph).expect("fixture graph published")
        };
        Self { peer, handle: registry.reclaim_authority_handle(), binding }
    }
    pub fn authority_handle(&self) -> crate::ubm_reclaim_authority::ReclaimAuthorityHandle { self.handle.clone() }
    pub fn binding(&self) -> GraphBinding { self.binding.clone() }
    pub fn acquire_reclaim(&self, binding: &GraphBinding, key: GraphKey) -> Result<PermitStatus, AuthorityError> { self.peer.acquire_reclaim(binding, key) }
    pub fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> { self.peer.commit_reclaim(key) }
    pub fn release_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> { self.peer.release_reclaim(key) }
    pub fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError> { self.peer.forget_reclaim(key) }
    pub fn invalidate_graph(&self) { self.peer.state.lock().expect("peer source lock poisoned").reclaim_binding.invalidate_graph(); }
}
