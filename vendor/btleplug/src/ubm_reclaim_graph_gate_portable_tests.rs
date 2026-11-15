// Run without platform dependencies:
// rustc --edition=2021 --test vendor/btleplug/src/ubm_reclaim_graph_gate_portable_tests.rs -o /tmp/ubm-reclaim-gate-tests
// /tmp/ubm-reclaim-gate-tests
#![allow(dead_code)]

// Match the shared authority's key boundary without pulling in a radio host.
mod ubm_reclaim_authority {
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct GraphKey {
        pub authority_id: u64,
        pub peer_id: String,
        pub connection_generation: String,
        pub graph: (String, u64, u64),
        pub permit_id: u64,
    }
}

#[path = "ubm_reclaim_graph_gate.rs"]
mod ubm_reclaim_graph_gate;
