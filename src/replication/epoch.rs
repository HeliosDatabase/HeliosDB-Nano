//! The replication epoch (spec P5.1 §1), as the HA code names it.
//!
//! The value, its key and its codec live in the storage layer
//! (`crate::storage::replication_epoch`), because the engine reads the
//! epoch at every open in every build — including a build without an HA
//! feature, which compiles no `replication` module. This module re-exports
//! it beside the code that writes it (the promotion), compares it (the
//! replication handshake, the archive chain, `recover`) and reports it.

pub use crate::storage::replication_epoch::*;
