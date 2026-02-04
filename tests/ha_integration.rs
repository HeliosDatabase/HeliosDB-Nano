//! High Availability Integration Tests
//!
//! Main entry point for HA integration test suite.
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

#![cfg(feature = "ha-tier1")]

mod ha_tests;
