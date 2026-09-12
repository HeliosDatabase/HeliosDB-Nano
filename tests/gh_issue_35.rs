//! GH #35 — opening a store must not re-execute DDL that is already applied.
//!
//! # The defect, and the two halves that fix it
//!
//! `recover_wal_at_open` replays every retained logical-WAL entry above the
//! durable `wal:checkpoint`. Before the fix the checkpoint advanced in exactly
//! ONE place — `truncate_to_checkpoint`, reached only from that same function —
//! so every entry a session wrote stayed above it until the NEXT open, and that
//! open re-executed the whole session: `DropTable`, `Truncate` and `RenameTable`
//! from history, against tables that legitimately exist now, with no `Insert`
//! entries to put the rows back (DDL is logged by default, autocommit DML is
//! not). That is the data-loss reproduction in the issue.
//!
//! The fix has two independent halves, and this file tests both:
//!
//! * **Half A — make the checkpoint advance.** At a clean close
//!   (`EmbeddedDatabase::drop`) the checkpoint is advanced to the current LSN
//!   after the row counters and index snapshots are durable, so a cleanly
//!   closed store leaves nothing the next open could mistake for redo.
//!   A periodic tick (`storage.wal_checkpoint_interval_entries` /
//!   `_interval_secs`) bounds the window a CRASH leaves. Tests:
//!   `*_survives_the_next_open` and
//!   `a_clean_close_must_not_leave_applied_ddl_above_the_checkpoint`.
//!
//! * **Half B — make replay fail closed.** A crash still leaves a genuine
//!   post-checkpoint window, and open recovery cannot tell an entry whose
//!   effect is already in the store (history) from one whose data write was
//!   lost (redo). With the default
//!   `storage.wal_replay_destructive_ddl = "refuse"`, `DropTable`, `Truncate`
//!   and `RenameTable` entries above the checkpoint are SKIPPED with an ERROR,
//!   leaving the store intact. Tests: `*_is_refused_when_replayed_as_history`
//!   and `a_replayed_rename_must_not_consume_a_recreated_source_table`.
//!
//! The two halves are complementary: Half A removes the hazard for the tidy
//! case, Half B is the floor under the crash case. A test that passes with only
//! one half is not sufficient, which is why the crash-shape tests disable the
//! close-time checkpoint (`wal_checkpoint_on_close = false`) and the clean-close
//! tests use the default configuration.
//!
//! # Positive controls (must pass before AND after)
//!
//! * `positive_control_a_checkpointed_store_reopens_unchanged`
//! * `positive_control_default_config_logs_ddl_but_not_autocommit_dml`
//! * `genuine_redo_past_the_checkpoint_still_works` — genuine redo for a
//!   NON-destructive entry above the checkpoint must still happen; a "fix" that
//!   simply stops replaying fails here.
//!
//! Every assertion is on DATA (a table's existence and its rows) or on the
//! on-disk checkpoint, never on a log line.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{
    config::DestructiveDdlReplay,
    storage::{WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase, Value,
};
use rocksdb::{Options, DB};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// `WriteAheadLog::CHECKPOINT_KEY` / `ENTRY_PREFIX`, restated so these tests
/// fail loudly if the on-disk names change without being revisited.
const CHECKPOINT_KEY: &[u8] = b"wal:checkpoint";
const ENTRY_PREFIX: &[u8] = b"wal:entries:";

// ---------------------------------------------------------------------------
// Harness (mirrors tests/wal_replay_upgrade_tests.rs)
// ---------------------------------------------------------------------------

fn config_for(dir: &Path, logical_wal_per_statement: bool, checkpoint_on_close: bool) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = logical_wal_per_statement;
    c.storage.wal_checkpoint_on_close = checkpoint_on_close;
    // Deterministic tests: the entry/time interval triggers must not reclaim
    // entries mid-test. The close-time trigger is what this file controls.
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c
}

/// The DEFAULT durability shape: DDL is logged, autocommit DML is not, and a
/// clean close checkpoints. This is what a real deployment runs.
fn open_db(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, false, true)).expect("open database")
}

fn open_db_logging_dml(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, true, true)).expect("open database")
}

/// The CRASH shape: a clean close does NOT checkpoint, so the session's entries
/// stay above the checkpoint exactly as a kill -9 would leave them. Used to
/// drive Half B (the fail-closed replay policy).
fn open_db_no_close_checkpoint(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, false, false)).expect("open database")
}

/// As above, with DML logged (for the genuine-redo control).
fn open_db_logging_dml_no_close_checkpoint(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, true, false)).expect("open database")
}

/// Open with an explicit destructive-replay policy (the escape hatch).
fn open_db_with_replay_policy(dir: &Path, policy: DestructiveDdlReplay) -> EmbeddedDatabase {
    let mut c = config_for(dir, false, true);
    c.storage.wal_replay_destructive_ddl = policy;
    EmbeddedDatabase::with_config(c).expect("open database")
}

/// Raw RocksDB handle on a CLOSED store. Retried: the previous handle's
/// background threads release the directory lock asynchronously.
fn open_raw(dir: &Path) -> Arc<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(false);
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));

    let mut last_err = None;
    for _ in 0..100 {
        match DB::open(&opts, dir) {
            Ok(db) => return Arc::new(db),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("raw RocksDB open failed: {last_err:?}");
}

fn checkpoint_of(db: &DB) -> Option<u64> {
    db.get(CHECKPOINT_KEY)
        .expect("read checkpoint")
        .map(|bytes| u64::from_le_bytes(bytes.as_slice().try_into().expect("checkpoint is 8 bytes")))
}

/// `(count, max_lsn)` of the retained `wal:entries:` keyspace.
fn retained_span(db: &DB) -> (usize, u64) {
    let mut count = 0usize;
    let mut max = 0u64;
    for item in db.prefix_iterator(ENTRY_PREFIX) {
        let Ok((key, _)) = item else { break };
        if !key.starts_with(ENTRY_PREFIX) {
            break;
        }
        count += 1;
        if let Ok(text) = std::str::from_utf8(&key[ENTRY_PREFIX.len()..]) {
            if let Ok(lsn) = text.parse::<u64>() {
                max = max.max(lsn);
            }
        }
    }
    (count, max)
}

/// Every retained operation, in LSN order, read through the ordinary WAL API.
///
/// Takes the caller's already-open handle: RocksDB permits exactly one handle
/// per directory, so opening a second one here would deadlock the test against
/// itself.
fn retained_ops(raw: &Arc<DB>) -> Vec<(u64, WalOperation)> {
    let wal = WriteAheadLog::open(Arc::clone(raw), WalSyncMode::Sync).expect("open wal");
    wal.replay()
        .expect("read the retained log")
        .into_iter()
        .map(|e| (e.lsn, e.operation))
        .collect()
}

/// One text column of a query, sorted; `None` when the query fails (e.g. the
/// table no longer exists — one of the outcomes GH#35 produces).
fn text_column_opt(db: &EmbeddedDatabase, sql: &str, column: usize) -> Option<Vec<String>> {
    let rows = db.query(sql, &[]).ok()?;
    let mut out: Vec<String> = rows
        .iter()
        .map(|tuple| match &tuple.values[column] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    out.sort();
    Some(out)
}

fn table_exists(db: &EmbeddedDatabase, table: &str) -> bool {
    db.query(&format!("SELECT * FROM {table}"), &[]).is_ok()
}

/// Drive a store into the CHECKPOINTED regime — the precondition for every
/// destructive test below.
///
/// Session 1 creates a table and inserts a row; the clean close runs Half A, so
/// the checkpoint must be stamped and the log reclaimed. Asserts both, so a
/// later test cannot pass vacuously on an un-checkpointed store.
fn seed_checkpointed_store(dir: &Path) {
    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create t");
        db.execute("INSERT INTO t VALUES (1, 'keep me')").expect("insert");
    }
    let raw = open_raw(dir);
    let cp = checkpoint_of(&raw).expect(
        "*** GH#35 Half A: a CLEAN close must stamp the checkpoint, else the next open \
         re-enters the replay arm ***",
    );
    let (count, max) = retained_span(&raw);
    assert!(
        count == 0 || max <= cp,
        "*** GH#35 Half A: a clean close left {count} applied entries above the checkpoint \
         (max LSN {max} > checkpoint {cp}) ***"
    );
}

/// The CRASH-shape precondition: session 2 runs with the close-time checkpoint
/// disabled, so its DDL is retained strictly above the seed's checkpoint.
/// Asserts the hazard really is on disk, so the replay tests below cannot pass
/// vacuously.
fn assert_hazard_retained_above_checkpoint(dir: &Path) -> Vec<(u64, WalOperation)> {
    let raw = open_raw(dir);
    let cp = checkpoint_of(&raw).expect("vacuity: the seed stamped the checkpoint");
    let (count, max) = retained_span(&raw);
    assert!(
        count > 0 && max > cp,
        "vacuity: session 2's DDL must be retained above the checkpoint ({max} > {cp}), \
         else this test exercises nothing"
    );
    retained_ops(&raw)
}

// ---------------------------------------------------------------------------
// 0. POSITIVE CONTROLS
// ---------------------------------------------------------------------------

/// The harness runs, a store round-trips, and repeated opens with no DDL after
/// the checkpoint are stable.
#[test]
fn positive_control_a_checkpointed_store_reopens_unchanged() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    for round in 1..=3 {
        let db = open_db(dir);
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["keep me".to_string()]),
            "row must survive open #{round}"
        );
    }
}

/// The anti-regression control: genuine logical redo must STILL happen for a
/// non-destructive entry above the checkpoint. Any fix for GH#35 that simply
/// stops replaying fails here.
///
/// The store is driven with `logical_wal_per_statement = true` and the close
/// checkpoint disabled, so DML lands in the logical WAL and stays there; the
/// row's data key is then deleted behind the engine's back and the checkpoint
/// planted just below that entry's LSN — the exact "entry landed, data write
/// did not" window redo exists for.
#[test]
fn genuine_redo_past_the_checkpoint_still_works() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db_logging_dml_no_close_checkpoint(dir);
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO r VALUES (1, 'alpha')").expect("insert 1");
        db.execute("INSERT INTO r VALUES (2, 'beta')").expect("insert 2");
    }
    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        let entries = wal.replay().expect("read log");
        let (lsn, data_key) = entries
            .iter()
            .rev()
            .find_map(|e| match &e.operation {
                WalOperation::Insert { table, key, .. } if table == "r" => Some((e.lsn, key.clone())),
                _ => None,
            })
            .expect("vacuity: the logical WAL must contain an Insert for `r`");
        assert!(
            data_key.starts_with(b"data:"),
            "vacuity: the logged Insert must carry the real storage key, got {:?}",
            String::from_utf8_lossy(&data_key)
        );
        assert!(
            raw.get(&data_key).expect("get").is_some(),
            "vacuity: the data key must exist before it is removed"
        );
        raw.delete(&data_key).expect("simulate the lost data write");
        raw.put(CHECKPOINT_KEY, (lsn - 1).to_le_bytes())
            .expect("plant checkpoint");
    }
    {
        let db = open_db_logging_dml(dir);
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM r ORDER BY id", 1),
            Some(vec!["alpha".to_string(), "beta".to_string()]),
            "an entry past the checkpoint was NOT replayed — crash recovery has become a no-op"
        );
    }
}

/// Under the DEFAULT configuration, DDL is written to the logical WAL and
/// autocommit DML is not. Pinned here so the asymmetry is a stated, tested
/// fact — and so nobody "fixes" GH#35 by flipping
/// `storage.logical_wal_per_statement` to `true`, which would put a synchronous
/// WAL append on every autocommit write while leaving the actual defect intact.
///
/// The close-time checkpoint is disabled so the log survives to be inspected;
/// the shape of the retained log is the subject, not the data.
#[test]
fn positive_control_default_config_logs_ddl_but_not_autocommit_dml() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db_no_close_checkpoint(dir);
        db.execute("CREATE TABLE asym (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO asym VALUES (1, 'row')").expect("insert");
        db.execute("DROP TABLE asym").expect("drop");
    }

    let raw = open_raw(dir);
    let ops = retained_ops(&raw);
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::CreateTable { table, .. } if table == "asym")),
        "CREATE TABLE must be logged regardless of `logical_wal_per_statement`, got {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "asym")),
        "DROP TABLE must be logged regardless of `logical_wal_per_statement`, got {ops:?}"
    );
    assert!(
        !ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::Insert { table, .. } if table == "asym")),
        "*** with `logical_wal_per_statement = false` (the default) an autocommit INSERT must \
         NOT be logged — if it is, this file's premise has changed. Got {ops:?} ***"
    );
}

// ---------------------------------------------------------------------------
// 1. HALF A — a CLEAN close must leave nothing above the checkpoint, and the
//    next open must preserve the live data.
// ---------------------------------------------------------------------------

/// Drop-and-recreate under the SAME name — the simplest destructive shape, and
/// an everyday migration step. Half A: the clean close advances the checkpoint,
/// so the next open replays nothing.
#[test]
fn drop_and_recreate_survives_the_next_open() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        db.execute("DROP TABLE t").expect("drop");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("recreate");
        db.execute("INSERT INTO t VALUES (1, 'new row')").expect("insert");
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["new row".to_string()]),
            "vacuity: the row must be there before the close"
        );
    }

    let db = open_db(dir);
    assert!(
        table_exists(&db, "t"),
        "*** GH#35: table `t` no longer exists after a plain reopen ***"
    );
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["new row".to_string()]),
        "*** GH#35: opening the store re-executed an already-applied `DROP TABLE t` and \
         destroyed the live table's rows ***"
    );
}

/// The production shape from the issue: the online-schema-change
/// create-copy-drop-rename swap, under a clean close.
#[test]
fn create_copy_drop_rename_swap_survives_the_next_open() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t_new (id INT PRIMARY KEY, v TEXT)")
            .expect("create t_new");
        db.execute("INSERT INTO t_new SELECT * FROM t").expect("copy rows");
        db.execute("DROP TABLE t").expect("drop t");
        db.execute("ALTER TABLE t_new RENAME TO t").expect("rename");
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["keep me".to_string()]),
            "vacuity: the swap must have preserved the row before the close"
        );
    }

    let db = open_db(dir);
    assert!(
        table_exists(&db, "t"),
        "*** GH#35: the swapped-in table `t` no longer exists after a plain reopen ***"
    );
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["keep me".to_string()]),
        "*** GH#35: opening the store replayed the maintenance swap's DDL and destroyed the \
         live table's rows ***"
    );
    assert!(
        !table_exists(&db, "t_new"),
        "*** GH#35: replay resurrected the `t_new` shell the rename consumed ***"
    );

    // The row-id counter must have survived too. A replayed rename can silently
    // reset a live table's counter, after which the next INSERT reuses a live
    // row_id and overwrites a row no PK/UNIQUE check can protect.
    db.execute("INSERT INTO t VALUES (2, 'written after the reopen')")
        .expect("insert after reopen");
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t ORDER BY id", 1),
        Some(vec!["keep me".to_string(), "written after the reopen".to_string()]),
        "*** GH#35: the first write after the reopen overwrote a live row — the replayed \
         RenameTable reset the target table's row-id counter ***"
    );
}

/// `TRUNCATE` under a clean close.
#[test]
fn truncate_then_repopulate_survives_the_next_open() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        // TRUNCATE is logged by the params family; the text family's inlined
        // arm does not call `log_truncate` (a GH#36 coverage gap), so this
        // hazard only exists for params-family truncates.
        db.execute_params("TRUNCATE TABLE t", &[]).expect("truncate");
        db.execute("INSERT INTO t VALUES (2, 'after truncate')")
            .expect("repopulate");
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["after truncate".to_string()]),
            "vacuity: the repopulated row must be there before the close"
        );
    }

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["after truncate".to_string()]),
        "*** GH#35: opening the store replayed an already-applied TRUNCATE and erased the \
         rows written after it ***"
    );
}

/// The invariant behind Half A, stated directly.
///
/// After a CLEAN close, the store must not be left holding entries whose
/// effects are already durable but which sit above the checkpoint — because the
/// next open cannot tell those apart from genuine redo.
#[test]
fn a_clean_close_must_not_leave_applied_ddl_above_the_checkpoint() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t2 (id INT PRIMARY KEY)").expect("create t2");
        db.execute("DROP TABLE t2").expect("drop t2");
        // Clean close: the Drop impl advances the checkpoint.
    }

    let raw = open_raw(dir);
    let cp = checkpoint_of(&raw).expect("vacuity: the store is checkpointed");
    let (count, max) = retained_span(&raw);
    assert!(
        count == 0 || max <= cp,
        "*** GH#35: a clean close left {count} already-applied WAL entries above the \
         checkpoint (max LSN {max} > checkpoint {cp}) ***"
    );
}

// ---------------------------------------------------------------------------
// 2. HALF B — the CRASH shape. The close-time checkpoint is disabled, so the
//    destructive DDL is retained above the checkpoint exactly as a kill -9
//    leaves it. Open recovery must REFUSE it by default.
// ---------------------------------------------------------------------------

/// The crash shape of drop-and-recreate. Replay must skip the `DropTable t`
/// entry (its effect may already be in the store) and leave the live rows.
#[test]
fn drop_and_recreate_is_refused_when_replayed_as_history() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db_no_close_checkpoint(dir);
        db.execute("DROP TABLE t").expect("drop");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("recreate");
        db.execute("INSERT INTO t VALUES (1, 'new row')").expect("insert");
    }

    let ops = assert_hazard_retained_above_checkpoint(dir);
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "t")),
        "vacuity: the retained log must hold `DropTable t`, got {ops:?}"
    );

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["new row".to_string()]),
        "*** GH#35 Half B: open recovery APPLIED a destructive `DropTable t` from history ***"
    );
}

/// The crash shape of the production swap.
#[test]
fn create_copy_drop_rename_swap_is_refused_when_replayed_as_history() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db_no_close_checkpoint(dir);
        db.execute("CREATE TABLE t_new (id INT PRIMARY KEY, v TEXT)")
            .expect("create t_new");
        db.execute("INSERT INTO t_new SELECT * FROM t").expect("copy rows");
        db.execute("DROP TABLE t").expect("drop t");
        db.execute("ALTER TABLE t_new RENAME TO t").expect("rename");
    }

    let ops = assert_hazard_retained_above_checkpoint(dir);
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "t")),
        "vacuity: the retained log must hold `DropTable t`, got {ops:?}"
    );

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["keep me".to_string()]),
        "*** GH#35 Half B: open recovery replayed the swap's destructive DDL and destroyed \
         the live table's rows ***"
    );
}

/// The crash shape of truncate-then-repopulate.
#[test]
fn truncate_then_repopulate_is_refused_when_replayed_as_history() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db_no_close_checkpoint(dir);
        // Params family: the text family's TRUNCATE arm does not log (GH#36).
        db.execute_params("TRUNCATE TABLE t", &[]).expect("truncate");
        db.execute("INSERT INTO t VALUES (2, 'after truncate')")
            .expect("repopulate");
    }

    let ops = assert_hazard_retained_above_checkpoint(dir);
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::Truncate { table } if table == "t")),
        "vacuity: the retained log must hold `Truncate t`, got {ops:?}"
    );

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec!["after truncate".to_string()]),
        "*** GH#35 Half B: open recovery replayed an already-applied TRUNCATE ***"
    );
}

/// `RenameTable` is the THIRD destructive replay arm and the one the issue's
/// own evidence does not contain. `Catalog::rename_table_inner` checks only
/// that the SOURCE exists — it never checks the TARGET — so a replayed rename
/// consumes a recreated source and clobbers the live target's rows. This test
/// contains NO `DropTable` and NO `Truncate`, so a fix that guards only those
/// two cannot pass it.
#[test]
fn a_replayed_rename_must_not_consume_a_recreated_source_table() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db_no_close_checkpoint(dir);
        // Cycle 1 of a rebuild job: build `src`, promote it to `dst`.
        db.execute("CREATE TABLE src (id INT PRIMARY KEY, v TEXT)")
            .expect("create src");
        db.execute("ALTER TABLE src RENAME TO dst").expect("promote src -> dst");
        db.execute("INSERT INTO dst VALUES (1, 'live')").expect("populate dst");
        // Cycle 2 starts: `src` is created again and populated. It is still
        // being built when the process is restarted.
        db.execute("CREATE TABLE src (id INT PRIMARY KEY, v TEXT)")
            .expect("recreate src");
        db.execute("INSERT INTO src VALUES (2, 'staging')")
            .expect("populate the new src");
    }

    let ops = assert_hazard_retained_above_checkpoint(dir);
    assert!(
        ops.iter().any(|(_, op)| matches!(
            op,
            WalOperation::RenameTable { old_table, new_table } if old_table == "src" && new_table == "dst"
        )),
        "vacuity: the retained log must hold `RenameTable src -> dst`, got {ops:?}"
    );
    assert!(
        !ops.iter().any(|(_, op)| matches!(op, WalOperation::DropTable { .. }))
            && !ops.iter().any(|(_, op)| matches!(op, WalOperation::Truncate { .. })),
        "vacuity: this test must contain NO DropTable/Truncate; got {ops:?}"
    );

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM dst", 1),
        Some(vec!["live".to_string()]),
        "*** GH#35 Half B: open recovery replayed an already-applied rename and overwrote \
         the live `dst` row with the staging table's ***"
    );
    assert!(
        table_exists(&db, "src"),
        "*** GH#35 Half B: the replayed rename consumed the recreated `src` table ***"
    );
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM src", 1),
        Some(vec!["staging".to_string()]),
        "*** GH#35 Half B: the replayed rename moved the recreated `src` table's rows away ***"
    );
}

/// The escape hatch: `wal_replay_destructive_ddl = "apply"` restores the
/// operator-driven redo — and therefore DOES destroy the live rows, which is
/// exactly why `refuse` is the default. Pins that the knob is wired.
#[test]
fn destructive_replay_policy_apply_still_applies() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db_no_close_checkpoint(dir);
        db.execute("DROP TABLE t").expect("drop");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("recreate");
        db.execute("INSERT INTO t VALUES (1, 'new row')").expect("insert");
    }
    assert_hazard_retained_above_checkpoint(dir);

    let db = open_db_with_replay_policy(dir, DestructiveDdlReplay::Apply);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t", 1),
        Some(vec![]),
        "*** with wal_replay_destructive_ddl = \"apply\" the replayed DropTable must remove \
         the live table's rows and leave the empty recreated shell ***"
    );
}
