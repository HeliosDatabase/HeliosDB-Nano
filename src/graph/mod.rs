//! Native graph adjacency-list storage and traversal.
//!
//! RAG-native (idea 1 of the integration plan).
//!
//! ## Design
//!
//! Edges are stored in two `DashMap`-backed adjacency lists -- one
//! forward (`from -> [(to, label, weight, props)]`) and one reverse
//! (`to -> [(from, ...)]`). This gives O(1) per-node neighbor lookup
//! and lets BFS / Dijkstra / A* run in tight loops without RocksDB
//! seeks per hop.
//!
//! Persistence is *out of scope* for this drop -- the existing
//! `StorageEngine` does not yet expose column families, and shoehorning
//! the graph CFs into the single-DB layout would require an engine-wide
//! migration. The graph store is therefore an in-process manager that
//! callers can warm from an external source (e.g. a SQL table) on
//! startup. See the plan note in external-project_INTEGRATION_PLAN.md.
//!
//! ## Public surface
//!
//! - [`storage::GraphStore`] -- thread-safe edge store
//! - [`traverse`] -- BFS, Dijkstra, bidirectional BFS, k-hop neighbors
//! - [`sql`] -- SQL function adapters (`graph_traverse`, `graph_shortest_path`)

pub mod sql;
pub mod storage;
pub mod traverse;

pub use storage::{Direction, Edge, EdgeId, GraphStore, NodeId};
pub use traverse::{Path, ShortestPathAlgorithm, TraversalLimits};
