//! High Availability Integration Tests
//!
//! These tests verify the HA replication system in various scenarios.
//! They can be run against local instances or Docker containers.
//!
//! # Running Tests
//!
//! ```bash
//! # Run all HA tests
//! cargo test --features ha-tier1 --test ha_integration
//!
//! # Run specific test
//! cargo test --features ha-tier1 --test ha_integration test_basic_replication
//! ```

mod cluster_tests;
mod failover_tests;
mod streaming_tests;
mod split_brain_tests;
