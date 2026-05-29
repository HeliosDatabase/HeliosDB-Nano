//! Integration Tests for Multi-User ACID In-Memory Mode (v3.1.0)
//!
//! This module contains comprehensive integration tests for the v3.1.0 upgrade.
//! These tests serve as both verification and specification (TDD approach).
//!
//! ## Test Organization
//!
//! - `session_management` - SessionManager tests
//! - `lock_management` - LockManager and deadlock tests
//! - `transaction_isolation` - Isolation level tests
//! - `dump_restore` - Dump/restore functionality
//! - `multi_user_scenarios` - End-to-end scenarios
//! - `performance` - Performance benchmarks
//! - `cli_commands` - CLI integration

mod cli_commands;
mod dump_restore;
mod lock_management;
mod multi_user_scenarios;
mod performance;
mod session_management;
mod transaction_isolation;
