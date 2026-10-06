//! Regression coverage for live WAL replay read visibility and row allocation.
//! Public APIs only; no HA global state, network, manually encoded tuples,
//! production paths, or secondary writable WAL appends. Plain INT rows only.
//! Captures the primary's actual SQL-generated logical WAL via replay_from.
use heliosdb_nano::{
    storage::{WalEntry, WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase,
};

type Rows = Vec<Vec<String>>;
const WARM: &str = "SELECT id, v FROM replay_visibility ORDER BY id";

fn db() -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Rows {
    db.query(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .into_iter()
        .map(|tuple| tuple.values.iter().map(ToString::to_string).collect())
        .collect()
}

fn expected(values: &[(i32, i32)], reversed: bool) -> Rows {
    values
        .iter()
        .map(|&(id, v)| {
            if reversed {
                vec![v.to_string(), id.to_string()]
            } else {
                vec![id.to_string(), v.to_string()]
            }
        })
        .collect()
}

fn raw(db: &EmbeddedDatabase) -> Rows {
    let mut result: Rows = db
        .storage
        .scan_table("replay_visibility")
        .unwrap()
        .into_iter()
        .map(|tuple| tuple.values.iter().map(ToString::to_string).collect())
        .collect();
    result.sort();
    result
}

fn pair() -> (EmbeddedDatabase, EmbeddedDatabase) {
    let primary = db();
    let standby = db();
    for node in [&primary, &standby] {
        node.execute("CREATE TABLE replay_visibility (id INT PRIMARY KEY, v INT)")
            .unwrap();
        node.execute("CREATE INDEX replay_visibility_v ON replay_visibility(v)")
            .unwrap();
        node.execute("INSERT INTO replay_visibility VALUES (1, 10)").unwrap();
        node.execute("INSERT INTO replay_visibility VALUES (2, 20)").unwrap();
        assert_eq!(raw(node), expected(&[(1, 10), (2, 20)], false));
    }
    (primary, standby)
}

// WriteAheadLog::open(Async) only reads the last LSN, has no background
// group-commit worker, and has no Drop implementation. This handle is used
// solely for the public, read-only replay_from method, and immediately dropped.
// It never appends, flushes, checkpoints, rotates, or truncates the primary WAL.
fn capture(primary: &EmbeddedDatabase, sql: &[&str]) -> Vec<WalEntry> {
    let before = primary.storage.wal_lsn().expect("logical WAL enabled");
    for statement in sql {
        primary.execute(statement).unwrap();
    }
    let reader = WriteAheadLog::open(primary.storage.db(), WalSyncMode::Async).unwrap();
    let entries = reader.replay_from(before).unwrap();
    assert!(!entries.is_empty(), "SQL must produce real WAL records");
    entries
}

fn apply(standby: &EmbeddedDatabase, entries: &[WalEntry]) {
    for entry in entries {
        standby
            .storage
            .apply_replicated_operation(entry.operation.clone())
            .unwrap_or_else(|error| panic!("apply LSN {} {:?}: {error}", entry.lsn, entry.operation));
    }
}

fn observe(errors: &mut Vec<String>, label: &str, actual: Rows, want: Rows) {
    if actual != want {
        errors.push(format!("{label}: expected {want:?}; actual {actual:?}"));
    }
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Insert,
    Update,
    Delete,
}

fn visibility(mutation: Mutation) {
    let (primary, standby) = pair();
    let initial = expected(&[(1, 10), (2, 20)], false);
    // Admission is currently second sighting. Five identical reads reliably
    // exercise the public repeated-query behavior without private cache hooks.
    for _ in 0..5 {
        assert_eq!(rows(&standby, WARM), initial);
    }
    let (sql, want, target) = match mutation {
        Mutation::Insert => (
            "INSERT INTO replay_visibility VALUES (3, 30)",
            vec![(1, 10), (2, 20), (3, 30)],
            3,
        ),
        Mutation::Update => (
            "UPDATE replay_visibility SET v = 11 WHERE id = 1",
            vec![(1, 11), (2, 20)],
            1,
        ),
        Mutation::Delete => ("DELETE FROM replay_visibility WHERE id = 1", vec![(2, 20)], 1),
    };
    let entries = capture(&primary, &[sql]);
    assert!(
        entries.iter().any(|entry| match (&entry.operation, mutation) {
            (WalOperation::Insert { table, .. }, Mutation::Insert)
            | (WalOperation::Update { table, .. }, Mutation::Update)
            | (WalOperation::Delete { table, .. }, Mutation::Delete) => table == "replay_visibility",
            _ => false,
        }),
        "the SQL fixture must exercise its named WAL variant: {entries:?}"
    );
    assert_eq!(raw(&primary), expected(&want, false));
    let mut errors = Vec::new();
    for pass in 1..=2 {
        // Repeat the exact captured operations, preserving keys/row IDs.
        // This is idempotent duplicate delivery, not out-of-order history replay.
        apply(&standby, &entries);
        let prefix = format!("{mutation:?}, application {pass}");
        observe(
            &mut errors,
            &format!("{prefix} raw storage"),
            raw(&standby),
            expected(&want, false),
        );
        observe(
            &mut errors,
            &format!("{prefix} identical warmed query"),
            rows(&standby, WARM),
            expected(&want, false),
        );
        // Each pass uses different SQL shapes so these probes have not been
        // result-cache warmed. The unchanged original above remains warmed.
        let reversed = pass == 1;
        let projection = if reversed { "v, id" } else { "id, v" };
        let scan = if reversed {
            "SELECT v, id FROM replay_visibility WHERE v >= 0 ORDER BY id"
        } else {
            "SELECT id, v FROM replay_visibility WHERE id > 0 ORDER BY id"
        };
        observe(
            &mut errors,
            &format!("{prefix} fresh scan"),
            rows(&standby, scan),
            expected(&want, reversed),
        );
        let count = if reversed {
            "SELECT COUNT(*) FROM replay_visibility"
        } else {
            "SELECT COUNT(*) FROM replay_visibility WHERE v >= 0"
        };
        observe(
            &mut errors,
            &format!("{prefix} fresh count"),
            rows(&standby, count),
            vec![vec![want.len().to_string()]],
        );
        let pk = format!("SELECT {projection} FROM replay_visibility WHERE id = {target}");
        let pk_want: Vec<_> = want.iter().copied().filter(|&(id, _)| id == target).collect();
        observe(
            &mut errors,
            &format!("{prefix} fresh PK"),
            rows(&standby, &pk),
            expected(&pk_want, reversed),
        );
        // Check both the removed/old secondary key and the new secondary key.
        for value in [10, 11, 30] {
            let secondary = format!("SELECT {projection} FROM replay_visibility WHERE v = {value} ORDER BY id");
            let secondary_want: Vec<_> = want.iter().copied().filter(|&(_, v)| v == value).collect();
            observe(
                &mut errors,
                &format!("{prefix} fresh secondary v={value}"),
                rows(&standby, &secondary),
                expected(&secondary_want, reversed),
            );
        }
    }
    assert!(errors.is_empty(), "replay visibility failures:\n{}", errors.join("\n"));
}

#[test]
fn replay_insert_is_visible_to_cached_and_fresh_sql() {
    visibility(Mutation::Insert);
}
#[test]
fn replay_update_is_visible_to_cached_and_fresh_sql() {
    visibility(Mutation::Update);
}
#[test]
fn replay_delete_is_visible_to_cached_and_fresh_sql() {
    visibility(Mutation::Delete);
}

fn continuation(explicit_transaction: bool) {
    let (primary, standby) = pair();
    let sql = if explicit_transaction {
        vec!["BEGIN", "INSERT INTO replay_visibility VALUES (3, 30)", "COMMIT"]
    } else {
        vec!["INSERT INTO replay_visibility VALUES (3, 30)"]
    };
    let entries = capture(&primary, &sql);
    assert!(
        entries.iter().any(|entry| matches!(&entry.operation,
        WalOperation::Insert { table, .. } | WalOperation::Update { table, .. }
        if table == "replay_visibility")),
        "must capture an inserted row: {entries:?}"
    );
    apply(&standby, &entries);
    apply(&standby, &entries);
    assert_eq!(raw(&standby), expected(&[(1, 10), (2, 20), (3, 30)], false));
    // Simulate the first local insert after promotion without mutating HA globals.
    // Capture/replay included every emitted counter record, not only row records.
    standby.execute("INSERT INTO replay_visibility VALUES (4, 40)").unwrap();
    assert_eq!(
        raw(&standby),
        expected(&[(1, 10), (2, 20), (3, 30), (4, 40)], false),
        "new local allocation must preserve every replayed row after duplicate delivery; WAL: {entries:?}"
    );
}

#[test]
fn autocommit_replay_preserves_row_counter_continuation() {
    continuation(false);
}
#[test]
fn explicit_transaction_replay_preserves_row_counter_continuation() {
    continuation(true);
}
