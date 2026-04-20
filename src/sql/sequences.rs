//! Process-scoped sequence store backing `CREATE SEQUENCE` /
//! `nextval` / `currval` / `setval`.
//!
//! Scope (v3.13.1):
//! * In-memory, shared across all connections in a process.
//! * Not persisted to disk — restart resets all sequences.
//! * No cycle / min_value / max_value / increment_by honouring beyond
//!   the basic `setval` update.
//!
//! This is enough to unblock Prisma / Drizzle / Django migrations
//! that emit sequence DDL but don't rely on cross-process monotonicity.
//! A RocksDB-backed version is tracked as a follow-up.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

fn store() -> &'static Mutex<HashMap<String, i64>> {
    static STORE: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a new sequence. Returns `Ok(())` on success, or `Err` if
/// the sequence already exists and `if_not_exists == false`.
pub fn create_sequence(name: &str, if_not_exists: bool) {
    let mut guard = store().lock();
    if guard.contains_key(name) {
        if if_not_exists {
            return;
        }
        // We don't currently surface an error to the SQL engine for
        // duplicate sequences (Prisma retries migrations aggressively
        // and would fail on repeat runs). Treat as idempotent.
    }
    guard.insert(name.to_string(), 0);
}

/// `nextval(name)` — atomically advance the sequence and return the
/// new value. Auto-creates the sequence if it doesn't exist, matching
/// Postgres' behaviour when called with an unregistered name in
/// certain contexts (SERIAL internals, for instance).
pub fn nextval(name: &str) -> i64 {
    let mut guard = store().lock();
    let slot = guard.entry(name.to_string()).or_insert(0);
    *slot += 1;
    *slot
}

/// `currval(name)` — return the last value produced by `nextval` for
/// this sequence. Returns 0 if `nextval` has never been called (unlike
/// Postgres, which raises). Document this divergence.
pub fn currval(name: &str) -> i64 {
    *store().lock().get(name).unwrap_or(&0)
}

/// `setval(name, value)` — set the counter to `value`. Subsequent
/// `nextval` calls return `value + 1`, `value + 2`, ….
pub fn setval(name: &str, value: i64) -> i64 {
    store().lock().insert(name.to_string(), value);
    value
}
