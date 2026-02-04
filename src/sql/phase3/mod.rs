//! Phase 3 SQL Extensions
//!
//! This module contains parsers for Phase 3 features:
//! - Database branching (CREATE/DROP/MERGE BRANCH)
//! - Time-travel queries (AS OF TIMESTAMP/TRANSACTION/SCN)
//! - Materialized views with advanced options
//! - System views and functions

pub mod branching;
pub mod time_travel;
pub mod materialized_views;
pub mod system_views;

pub use branching::BranchingParser;
pub use time_travel::TimeTravelParser;
pub use materialized_views::MaterializedViewParser;
pub use system_views::SystemViewRegistry;
