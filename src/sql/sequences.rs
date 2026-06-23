//! Process-scoped sequence store backing `CREATE SEQUENCE` /
//! `nextval` / `currval` / `setval`.
//!
//! Scope:
//! * In-memory, shared across all connections in a process.
//! * Not persisted to disk — restart resets all sequences.
//! * Honours `START WITH` and `INCREMENT BY`; `MINVALUE` / `MAXVALUE` /
//!   `CYCLE` are parsed but not yet enforced.
//!
//! This is enough to unblock Prisma / Drizzle / Django / Ora2Pg migrations
//! that emit sequence DDL but don't rely on cross-process monotonicity.
//! A RocksDB-backed version is tracked as a follow-up.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

/// Per-sequence state: the last value produced (`current`) and the step
/// applied by each `nextval`. `START WITH s` is stored by initialising
/// `current = s - increment`, so the first `nextval` returns exactly `s`.
#[derive(Clone, Copy, Debug)]
struct SeqMeta {
    current: i64,
    increment: i64,
}

impl Default for SeqMeta {
    fn default() -> Self {
        Self {
            current: 0,
            increment: 1,
        }
    }
}

fn store() -> &'static Mutex<HashMap<String, SeqMeta>> {
    static STORE: OnceLock<Mutex<HashMap<String, SeqMeta>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a new sequence, honouring `START WITH` / `INCREMENT BY`.
/// `start` defaults to 1 and `increment` to 1 when the DDL omits them —
/// which reproduces the previous `nextval` → 1, 2, 3 … behaviour exactly.
pub fn create_sequence(name: &str, if_not_exists: bool, start: Option<i64>, increment: Option<i64>) {
    // PostgreSQL rejects INCREMENT 0; fall back to 1 rather than erroring.
    let increment = match increment.unwrap_or(1) {
        0 => 1,
        n => n,
    };
    let start = start.unwrap_or(1);
    let mut guard = store().lock();
    if guard.contains_key(name) && if_not_exists {
        return;
    }
    // We don't surface a duplicate-sequence error (Prisma retries
    // migrations aggressively and would fail on repeat runs). Treat a
    // re-create as an idempotent reset. The first `nextval` returns `start`.
    guard.insert(
        name.to_string(),
        SeqMeta {
            current: start.saturating_sub(increment),
            increment,
        },
    );
}

/// `nextval(name)` — advance by the sequence's increment and return the
/// new value. Auto-creates with the default (start 1, increment 1) if the
/// name is unknown, matching the prior behaviour relied on by SERIAL
/// internals and lenient migrations.
pub fn nextval(name: &str) -> i64 {
    let mut guard = store().lock();
    let slot = guard.entry(name.to_string()).or_default();
    slot.current += slot.increment;
    slot.current
}

/// `currval(name)` — return the last value produced by `nextval` for this
/// sequence. Returns 0 if the sequence is unknown or `nextval` has never
/// been called (unlike Postgres, which raises). Document this divergence.
pub fn currval(name: &str) -> i64 {
    store().lock().get(name).map_or(0, |m| m.current)
}

/// `setval(name, value [, is_called])` — set the sequence counter, matching
/// PostgreSQL's two- and three-argument forms.
///
/// * `is_called = true` (the default, and the two-argument form): `value` is
///   treated as *already produced*, so the next `nextval` returns
///   `value + increment`, `value + 2*increment`, ….
/// * `is_called = false`: `value` has *not* been produced yet, so the next
///   `nextval` returns exactly `value`.
///
/// Always returns `value`, like PostgreSQL.
pub fn setval(name: &str, value: i64, is_called: bool) -> i64 {
    let mut guard = store().lock();
    let entry = guard.entry(name.to_string()).or_default();
    entry.current = if is_called {
        value
    } else {
        // Back the counter off by one increment so the next nextval lands on
        // `value` exactly. saturating_sub guards the i64::MIN edge.
        value.saturating_sub(entry.increment)
    };
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sequence_starts_at_one() {
        create_sequence("seq_default", false, None, None);
        assert_eq!(nextval("seq_default"), 1);
        assert_eq!(nextval("seq_default"), 2);
        assert_eq!(currval("seq_default"), 2);
    }

    #[test]
    fn honors_start_and_increment() {
        create_sequence("seq_si", false, Some(100), Some(10));
        assert_eq!(nextval("seq_si"), 100);
        assert_eq!(nextval("seq_si"), 110);
        assert_eq!(nextval("seq_si"), 120);
    }

    #[test]
    fn setval_preserves_increment() {
        create_sequence("seq_sv", false, Some(1), Some(5));
        assert_eq!(nextval("seq_sv"), 1);
        // Two-arg form == is_called=true: next nextval is value + increment.
        setval("seq_sv", 50, true);
        assert_eq!(nextval("seq_sv"), 55);
    }

    #[test]
    fn setval_is_called_false_makes_next_nextval_equal_value() {
        create_sequence("seq_sv_false", false, Some(1), Some(5));
        assert_eq!(nextval("seq_sv_false"), 1);
        // is_called=false: the next nextval returns exactly `value`.
        setval("seq_sv_false", 300, false);
        assert_eq!(nextval("seq_sv_false"), 300);
        // and then resumes stepping by the increment.
        assert_eq!(nextval("seq_sv_false"), 305);
    }

    #[test]
    fn unknown_sequence_auto_vivifies_at_one() {
        // Preserves the lenient pre-existing behaviour SERIAL internals rely on.
        assert_eq!(nextval("seq_never_created_xyz"), 1);
    }
}
