//! GH #35 — opening a store must not re-execute DDL that is already applied.
//!
//! Intended file name: `tests/gh_issue_35.rs`.
//!
//! # What 57c808b fixed, and what it did not
//!
//! `StorageEngine::recover_wal_at_open` (`src/storage/engine.rs:2463`) now
//! classifies the store before replaying:
//!
//! | `wal:checkpoint` | retained entries | action |
//! |---|---|---|
//! | ABSENT            | some | adopt, do NOT replay  (engine.rs:2492) |
//! | present, `>= max` | some | reclaim, do NOT replay (engine.rs:2504) |
//! | present, `< max`  | some | **REPLAY every entry above the checkpoint** (engine.rs:2515) |
//!
//! The first row is the legacy 3.x → 4.x upgrade and is genuinely fixed;
//! `tests/wal_replay_upgrade_tests.rs` pins it.
//!
//! The third row is what these tests are about. The checkpoint advances in
//! EXACTLY ONE place — `truncate_to_checkpoint`, called only from
//! `recover_wal_at_open` (engine.rs:2496 / 2511 / 2533). Nothing advances it
//! while the process runs, and `impl Drop for EmbeddedDatabase`
//! (`src/lib.rs:820`) does not advance it at close either. So every logical-WAL
//! entry a session writes stays strictly above the checkpoint until the NEXT
//! open, and that next open replays all of them — including DDL.
//!
//! DDL logging is NOT gated on `storage.logical_wal_per_statement`
//! (`log_create_table` / `log_drop_table` / `log_rename_table` /
//! `log_truncate`, engine.rs:9902-9971), while autocommit DML IS
//! (`config.rs:666` defaults it to `false`). A default-configured store
//! therefore retains a DDL-only log, and re-executing a DDL-only log against a
//! state those statements already produced is destructive in exactly the way
//! the issue describes: the second open re-runs `DROP TABLE` against the table
//! that is live NOW, and no `Insert` entries exist to put the rows back.
//!
//! `apply_wal_operation`'s `DropTable` arm (engine.rs:10456) drops the table
//! whenever it exists; `warn_if_replayed_ddl_destroys_rows` (engine.rs:10854)
//! only WARNs first.
//!
//! # Expected outcome on the tree as of `e6ed61f` (v4.31.1)
//!
//!   PASS  `positive_control_a_checkpointed_store_reopens_unchanged`
//!   PASS  `positive_control_default_config_logs_ddl_but_not_autocommit_dml`
//!   PASS  `genuine_redo_past_the_checkpoint_still_works`  (anti-regression control)
//!   FAIL  `drop_and_recreate_survives_the_next_open`
//!   FAIL  `create_copy_drop_rename_swap_survives_the_next_open`
//!   FAIL  `truncate_then_repopulate_survives_the_next_open`
//!   FAIL  `a_replayed_rename_must_not_consume_a_recreated_source_table`
//!   FAIL  `a_clean_close_must_not_leave_applied_ddl_above_the_checkpoint`
//!
//! `a_replayed_rename_…` deliberately contains NO `DropTable` and NO
//! `Truncate`: a fix that guards only those two arms still fails it, because
//! `RenameTable` is destructive when replayed as history too
//! (`Catalog::rename_table_inner`, src/storage/catalog.rs:2077, never checks
//! whether the rename TARGET already exists).
//!
//! `a_clean_close_must_not_leave_applied_ddl_above_the_checkpoint` asserts the
//! MECHANISM (the checkpoint must advance at a clean close) rather than data.
//! It is legitimate only because advancing the checkpoint is a required half of
//! the fix, not an optional one: without it every restart re-enters the replay
//! arm and the store depends forever on the non-destructive-replay guard
//! holding for every DDL variant, present and future.
//!
//! Every assertion is on DATA (a table's existence and its rows) or on the
//! on-disk checkpoint, never on a log line.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{
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

fn config_for(dir: &Path, logical_wal_per_statement: bool) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = logical_wal_per_statement;
    c
}

/// The DEFAULT durability shape: DDL is logged, autocommit DML is not. This is
/// what a real deployment runs, and it is the configuration in which replaying
/// history destroys rows with nothing to restore them.
fn open_db(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, false)).expect("open database")
}

fn open_db_logging_dml(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, true)).expect("open database")
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
/// destructive test below. Returns nothing; asserts the precondition itself.
///
/// Session 1 creates a table (a logged `CreateTable`), so the FIRST reopen
/// takes the legacy-adopt arm and stamps `wal:checkpoint`. From then on the
/// store is "trusted" and the replay arm is live.
fn seed_checkpointed_store(dir: &Path) {
    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create t");
        db.execute("INSERT INTO t VALUES (1, 'keep me')").expect("insert");
    }
    {
        let raw = open_raw(dir);
        assert_eq!(
            checkpoint_of(&raw),
            None,
            "vacuity: a store that has never been reopened must be un-checkpointed"
        );
        assert!(
            retained_span(&raw).0 > 0,
            "vacuity: the CREATE TABLE must have been logged, else the reopen stamps nothing"
        );
    }
    {
        // The legacy-adopt open. Nothing is replayed; the checkpoint is stamped.
        let db = open_db(dir);
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["keep me".to_string()]),
            "the legacy-adopt open must preserve the seeded row (this half IS fixed)"
        );
    }
    let raw = open_raw(dir);
    assert!(
        checkpoint_of(&raw).is_some(),
        "vacuity: the store must now be CHECKPOINTED — otherwise the tests below would be \
         exercising the legacy-adopt arm, which is already fixed, and would pass vacuously"
    );
    // The adopt must have reclaimed the log. Stated as "nothing is left ABOVE
    // the checkpoint" rather than "the log is empty": an open may legitimately
    // append system-table DDL of its own, and an exact-zero assertion would
    // then fail for a reason that has nothing to do with GH#35. What the tests
    // below actually need is that the store is CHECKPOINTED and that any
    // destructive entry they go on to create provably sits above that mark —
    // which each of them asserts for itself.
    let cp = checkpoint_of(&raw).expect("checkpointed");
    let (count, max) = retained_span(&raw);
    assert!(
        count == 0 || max <= cp,
        "vacuity: the adopt must have reclaimed the log; {count} entries remain with max LSN \
         {max} above checkpoint {cp}"
    );
}

// ---------------------------------------------------------------------------
// 0. POSITIVE CONTROLS
// ---------------------------------------------------------------------------

/// The harness runs, a store round-trips, and repeated opens with no DDL after
/// the checkpoint are stable. Passes before and after any fix.
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

/// The anti-regression control: genuine logical redo must STILL happen for an
/// entry above the checkpoint. Any fix for GH#35 that simply stops replaying
/// fails here.
///
/// The store is driven with `logical_wal_per_statement = true` so DML lands in
/// the logical WAL; the row's data key is then deleted behind the engine's back
/// and the checkpoint planted just below that entry's LSN — the exact
/// "entry landed, data write did not" window redo exists for.
#[test]
fn genuine_redo_past_the_checkpoint_still_works() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db_logging_dml(dir);
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

// ---------------------------------------------------------------------------
// 1. THE REGRESSION. Post-checkpoint DDL is re-executed on the next open.
// ---------------------------------------------------------------------------

/// Drop-and-recreate under the SAME name — the simplest destructive shape, and
/// an everyday migration step.
///
/// Session 2 (on an already-checkpointed store) runs
/// `DROP TABLE t; CREATE TABLE t (…); INSERT …`. The retained log is
/// `[DropTable t, CreateTable t]` (autocommit DML is not logged by default), all
/// above the checkpoint. On the next open the engine replays it: `DropTable t`
/// deletes the table that is LIVE NOW together with its rows, `CreateTable t`
/// puts an empty shell back. Nothing restores the rows.
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

    // Vacuity: the hazard really is present on disk.
    {
        let raw = open_raw(dir);
        let cp = checkpoint_of(&raw).expect("vacuity: the store is checkpointed");
        let (count, max) = retained_span(&raw);
        assert!(count > 0, "vacuity: session 2's DDL must be retained");
        assert!(
            max > cp,
            "vacuity: the retained DDL must sit ABOVE the checkpoint ({max} > {cp}), else this \
             test exercises nothing"
        );
        let ops = retained_ops(&raw);
        assert!(
            ops.iter()
                .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "t")),
            "vacuity: the retained log must hold `DropTable t`, got {ops:?}"
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

/// The production shape from the issue, verbatim: the online-schema-change
/// create-copy-drop-rename swap.
///
/// The retained log after session 2 is
/// `[CreateTable t_new, DropTable t, RenameTable t_new -> t]`. Replaying it
/// against the post-swap state: `CreateTable t_new` finds no `t_new` (it was
/// renamed) and creates an EMPTY one; `DropTable t` finds the live, populated
/// `t` and deletes it; `RenameTable t_new -> t` moves the empty shell into
/// place. The table survives by name and every row is gone — the exact
/// keyspace diff the issue reports (`meta:table:organizations` REMOVED,
/// `meta:table:organizations_new` ADDED).
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

    {
        let raw = open_raw(dir);
        let cp = checkpoint_of(&raw).expect("vacuity: checkpointed");
        let (count, max) = retained_span(&raw);
        assert!(
            count > 0 && max > cp,
            "vacuity: the swap's DDL must be retained above {cp}"
        );
        let ops = retained_ops(&raw);
        assert!(
            ops.iter()
                .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "t")),
            "vacuity: the retained log must hold `DropTable t`, got {ops:?}"
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

    // The row-id counter must have survived too. `rename_table_inner`
    // (src/storage/catalog.rs:2077) copies the SOURCE table's `counter:{t}`
    // onto the target and never checks whether the target already exists, so a
    // replayed rename can silently reset a live table's counter to 0 — after
    // which the next INSERT reuses a live row_id and overwrites a row that no
    // PK/UNIQUE check can protect (row_id is not a user column). A fix that
    // only stops `DropTable`/`Truncate` from being replayed leaves exactly this
    // behind, and every assertion above would still pass.
    db.execute("INSERT INTO t VALUES (2, 'written after the reopen')")
        .expect("insert after reopen");
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM t ORDER BY id", 1),
        Some(vec!["keep me".to_string(), "written after the reopen".to_string()]),
        "*** GH#35: the first write after the reopen overwrote a live row — the replayed \
         RenameTable reset the target table's row-id counter ***"
    );
}

/// `RenameTable` is the THIRD destructive replay arm, and the one the issue's
/// own evidence does not contain (the production log had no rename, because the
/// older engine did not log one — see the companion fidelity issue). It matters
/// now precisely because the current engine DOES log renames.
///
/// `Catalog::rename_table_inner` (src/storage/catalog.rs:2077) checks only that
/// the SOURCE exists. It never checks the TARGET: it batch-writes the source's
/// `meta:table:`/`counter:` records over the target's and moves every
/// `data:{src}:{row_id}` key to `data:{dst}:{row_id}`, clobbering any live row
/// that happens to share a row_id. Replayed as history against a store where
/// the source name has since been RECREATED — the shape of any recurring
/// rebuild-and-swap job — it consumes the live source table and corrupts the
/// live target.
///
/// This test exists to stop a fix that only guards `DropTable` and `Truncate`:
/// it contains neither.
#[test]
fn a_replayed_rename_must_not_consume_a_recreated_source_table() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
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

    {
        let raw = open_raw(dir);
        let cp = checkpoint_of(&raw).expect("vacuity: checkpointed");
        let (count, max) = retained_span(&raw);
        assert!(count > 0 && max > cp, "vacuity: the rename must be retained above {cp}");
        let ops = retained_ops(&raw);
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
            "vacuity: this test must contain NO DropTable/Truncate, so a fix that only guards \
             those two cannot make it pass; got {ops:?}"
        );
    }

    let db = open_db(dir);
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM dst", 1),
        Some(vec!["live".to_string()]),
        "*** GH#35: opening the store replayed an already-applied `ALTER TABLE src RENAME TO \
         dst` and overwrote the live `dst` row with the staging table's ***"
    );
    assert!(
        table_exists(&db, "src"),
        "*** GH#35: the replayed rename consumed the recreated `src` table ***"
    );
    assert_eq!(
        text_column_opt(&db, "SELECT id, v FROM src", 1),
        Some(vec!["staging".to_string()]),
        "*** GH#35: the replayed rename moved the recreated `src` table's rows away ***"
    );
}

/// `TRUNCATE` is the other destructive operation the replay driver applies
/// (`apply_wal_operation`'s Truncate arm, engine.rs:10501). Rows inserted AFTER
/// the truncate are erased by re-running it.
#[test]
fn truncate_then_repopulate_survives_the_next_open() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        db.execute("TRUNCATE TABLE t").expect("truncate");
        db.execute("INSERT INTO t VALUES (2, 'after truncate')")
            .expect("repopulate");
        assert_eq!(
            text_column_opt(&db, "SELECT id, v FROM t", 1),
            Some(vec!["after truncate".to_string()]),
            "vacuity: the repopulated row must be there before the close"
        );
    }

    {
        let raw = open_raw(dir);
        let cp = checkpoint_of(&raw).expect("vacuity: checkpointed");
        let (count, max) = retained_span(&raw);
        assert!(
            count > 0 && max > cp,
            "vacuity: the TRUNCATE must be retained above {cp}"
        );
        let ops = retained_ops(&raw);
        assert!(
            ops.iter()
                .any(|(_, op)| matches!(op, WalOperation::Truncate { table } if table == "t")),
            "vacuity: the retained log must hold `Truncate t`, got {ops:?}"
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

// ---------------------------------------------------------------------------
// 2. THE MECHANISM. The checkpoint never advances except at open.
// ---------------------------------------------------------------------------

/// The invariant behind all three failures above, stated directly.
///
/// After a CLEAN close, the store must not be left holding entries whose
/// effects are already durable in the data keyspace but which sit above the
/// checkpoint — because the next open cannot tell those apart from genuine
/// redo and will re-execute them. Either the log is empty or the checkpoint
/// covers it.
///
/// Today the checkpoint moves in exactly one place — `truncate_to_checkpoint`,
/// reached only from `recover_wal_at_open` — so this fails with the whole of
/// session 2's log sitting above the checkpoint.
#[test]
fn a_clean_close_must_not_leave_applied_ddl_above_the_checkpoint() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    seed_checkpointed_store(dir);

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t2 (id INT PRIMARY KEY)").expect("create t2");
        db.execute("DROP TABLE t2").expect("drop t2");
        // Clean close: the Drop impl runs, flushing counters and index snapshots.
    }

    let raw = open_raw(dir);
    let cp = checkpoint_of(&raw).expect("vacuity: the store is checkpointed");
    let (count, max) = retained_span(&raw);
    assert!(
        count == 0 || max <= cp,
        "*** GH#35: a clean close left {count} already-applied WAL entries above the \
         checkpoint (max LSN {max} > checkpoint {cp}). The next open cannot distinguish them \
         from redo and will re-execute the DDL. ***"
    );
}

// ---------------------------------------------------------------------------
// 3. THE ASYMMETRY THAT MAKES REPLAY DESTRUCTIVE — a control, not a defect.
// ---------------------------------------------------------------------------

/// Under the DEFAULT configuration, DDL is written to the logical WAL and
/// autocommit DML is not. That asymmetry is the whole reason replaying a
/// retained log destroys data instead of merely repeating it: there are
/// `DropTable` / `Truncate` / `RenameTable` entries to re-execute and no
/// `Insert` entries to put the rows back.
///
/// Pinned here so the asymmetry is a stated, tested fact rather than an
/// inference — and, more importantly, so nobody "fixes" GH#35 by flipping
/// `storage.logical_wal_per_statement` to `true`. That would make the three
/// destructive tests above pass (replay would re-insert the rows it destroyed)
/// while leaving the actual defect — re-executing applied DDL on open —
/// completely intact, and would put a synchronous WAL append on every
/// autocommit write.
///
/// PASSES on the current tree and must keep passing. It asserts on the shape of
/// the log rather than on data, which is legitimate precisely because it is
/// documenting a mechanism, not standing in for a data assertion.
#[test]
fn positive_control_default_config_logs_ddl_but_not_autocommit_dml() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db(dir);
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
        "CREATE TABLE must be logged regardless of `logical_wal_per_statement` \
         (src/storage/engine.rs:9902 gates only on `self.wal.is_some()`), got {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::DropTable { table } if table == "asym")),
        "DROP TABLE must be logged regardless of `logical_wal_per_statement` \
         (src/storage/engine.rs:9924), got {ops:?}"
    );
    assert!(
        !ops.iter()
            .any(|(_, op)| matches!(op, WalOperation::Insert { table, .. } if table == "asym")),
        "*** with `logical_wal_per_statement = false` (the default, src/config.rs:666) an \
         autocommit INSERT must NOT be logged — if it is, this file's premise has changed and \
         every FAIL expectation in it must be re-derived. Got {ops:?} ***"
    );
}
