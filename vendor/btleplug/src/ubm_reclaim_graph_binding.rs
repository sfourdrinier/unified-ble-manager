// vendor/btleplug/src/ubm_reclaim_graph_binding.rs
//! Opaque discovery-time binding to one native physical owner and graph.

use std::sync::Arc;

/// Retained by the host with the exact discovery result. Its physical token is
/// issued by the native source and cannot be reconstructed from peer strings.
#[derive(Clone, Debug)]
pub struct GraphBinding {
    source: Arc<()>,
    connection: Arc<()>,
    peer_id: String,
    connection_generation: String,
    graph: (String, u64, u64),
}
impl GraphBinding {
    /// Exact captured source and physical epoch, including the host identity.
    pub fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
            && Arc::ptr_eq(&self.connection, &other.connection)
            && self.peer_id == other.peer_id
            && self.connection_generation == other.connection_generation
            && self.graph == other.graph
    }
}

/// The platform must mutate and inspect this only under its physical source
/// and discovery-graph lock. A disconnect retires the old connection token.
pub struct GraphBindingOwner {
    source: Arc<()>,
    connection: Option<Arc<()>>,
    graph: Option<(String, u64, u64)>,
}

impl Default for GraphBindingOwner {
    fn default() -> Self {
        Self { source: Arc::new(()), connection: None, graph: None }
    }
}

impl GraphBindingOwner {
    /// Source ownership remains authentic even after the physical epoch retires.
    pub fn owns(&self, binding: &GraphBinding) -> bool { Arc::ptr_eq(&self.source, &binding.source) }
    pub fn attach(&mut self) {
        self.connection = Some(Arc::new(()));
        self.graph = None;
    }

    pub fn disconnect(&mut self) {
        self.connection = None;
        self.graph = None;
    }

    pub fn invalidate_graph(&mut self) {
        self.graph = None;
    }

    /// Called with the native graph publication; an unattached owner refuses it.
    pub fn publish(&mut self, graph: (String, u64, u64)) -> bool {
        if self.connection.is_none() { return false; }
        self.graph = Some(graph);
        true
    }

    /// The caller holds the central connection admission while capturing this.
    pub fn capture(&self, peer_id: String, connection_generation: String, graph: &(String, u64, u64)) -> Option<GraphBinding> {
        let connection = self.connection.as_ref()?;
        if self.graph.as_ref() != Some(graph) { return None; }
        Some(GraphBinding { source: self.source.clone(), connection: connection.clone(), peer_id, connection_generation, graph: graph.clone() })
    }

    /// Must run in the same source transaction that acquires the graph permit.
    pub fn validates(&self, binding: &GraphBinding, peer_id: &str, connection_generation: &str, graph: &(String, u64, u64)) -> bool {
        let Some(connection) = &self.connection else { return false; };
        Arc::ptr_eq(&self.source, &binding.source)
            && Arc::ptr_eq(connection, &binding.connection)
            && self.graph.as_ref() == Some(graph)
            && binding.graph == *graph
            && binding.peer_id == peer_id
            && binding.connection_generation == connection_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_reconnect_and_foreign_source_reject_old_binding() {
        let graph = ("native".to_owned(), 7, 1);
        let mut owner = GraphBindingOwner::default();
        assert!(!owner.publish(graph.clone()));
        owner.attach(); assert!(owner.publish(graph.clone()));
        let binding = owner.capture("peer".into(), "central-1".into(), &graph).unwrap();
        assert!(owner.validates(&binding, "peer", "central-1", &graph));
        assert!(!owner.validates(&binding, "peer", "central-2", &graph));
        let mut other = GraphBindingOwner::default(); other.attach(); other.publish(graph.clone());
        assert!(!other.validates(&binding, "peer", "central-1", &graph));
        owner.disconnect();
        assert!(!owner.validates(&binding, "peer", "central-1", &graph));
        owner.attach(); owner.publish(graph.clone());
        assert!(!owner.validates(&binding, "peer", "central-1", &graph));
        let current = owner.capture("peer".into(), "central-2".into(), &graph).unwrap();
        let newer = ("native".to_owned(), 7, 2);
        owner.publish(newer.clone());
        assert!(!owner.validates(&current, "peer", "central-2", &graph));
        assert!(owner.capture("peer".into(), "central-2".into(), &newer).is_some());
    }
}
