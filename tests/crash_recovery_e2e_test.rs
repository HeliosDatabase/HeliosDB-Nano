//! End-to-end crash recovery integration test
//!
//! Tests that data survives a simulated crash (unclean shutdown) by:
//! 1. Opening a disk-backed database
//! 2. Inserting data
//! 3. Dropping the database without clean shutdown (simulating crash)
//! 4. Reopening the database
//! 5. Verifying data is recoverable via WAL auto-replay

use heliosdb_nano::EmbeddedDatabase;
use std::path::Path;
use tempfile::TempDir;

/// Helper to create a disk-backed database in a temp directory
fn open_db(dir: &std::path::Path) -> EmbeddedDatabase {
    // Default config has WAL enabled with Sync mode
    EmbeddedDatabase::new(dir).expect("Failed to open database")
}

#[test]
fn test_crash_recovery_insert_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Insert data and "crash" (drop without clean shutdown)
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE crash_test (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO crash_test VALUES (1, 'Alice')").unwrap();
        db.execute("INSERT INTO crash_test VALUES (2, 'Bob')").unwrap();
        db.execute("INSERT INTO crash_test VALUES (3, 'Charlie')").unwrap();

        // Verify data is readable before crash
        let rows = db.query("SELECT id, name FROM crash_test", &[]).unwrap();
        assert_eq!(rows.len(), 3, "Should have 3 rows before crash");

        // Drop without calling any shutdown method = simulated crash
        drop(db);
    }

    // Phase 2: Reopen database — WAL auto-replay should recover data
    {
        let db = open_db(&db_path);

        // Schema should survive (stored in RocksDB metadata)
        let rows = db.query("SELECT id, name FROM crash_test", &[]).unwrap();
        assert!(
            !rows.is_empty(),
            "Table should exist and have data after crash recovery"
        );
        // Data should survive via RocksDB + WAL
        assert_eq!(rows.len(), 3, "Should have 3 rows after crash recovery");
    }
}

#[test]
fn test_crash_recovery_transaction_committed() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Use explicit transaction, commit, then crash
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE txn_test (id INT, val TEXT)").unwrap();

        // Committed transaction — should survive
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO txn_test VALUES (1, 'committed')").unwrap();
        db.execute("COMMIT").unwrap();

        drop(db);
    }

    // Phase 2: Verify committed data survived
    {
        let db = open_db(&db_path);
        let rows = db.query("SELECT val FROM txn_test WHERE id = 1", &[]).unwrap();
        assert_eq!(rows.len(), 1, "Committed transaction data should survive crash");
    }
}

#[test]
fn test_crash_recovery_multiple_tables() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Create multiple tables with data
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id INT, user_id INT, amount INT)")
            .unwrap();

        for i in 1..=10 {
            db.execute(&format!("INSERT INTO users VALUES ({}, 'user_{}')", i, i))
                .unwrap();
        }
        for i in 1..=20 {
            db.execute(&format!(
                "INSERT INTO orders VALUES ({}, {}, {})",
                i,
                (i % 10) + 1,
                i * 100
            ))
            .unwrap();
        }

        drop(db);
    }

    // Phase 2: Verify all tables recovered
    {
        let db = open_db(&db_path);

        let users = db.query("SELECT id FROM users", &[]).unwrap();
        assert_eq!(users.len(), 10, "All 10 users should survive crash");

        let orders = db.query("SELECT id FROM orders", &[]).unwrap();
        assert_eq!(orders.len(), 20, "All 20 orders should survive crash");
    }
}

#[test]
fn test_crash_recovery_update_delete() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Insert, update, delete, then crash
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE crud_test (id INT, status TEXT)").unwrap();
        db.execute("INSERT INTO crud_test VALUES (1, 'initial')").unwrap();
        db.execute("INSERT INTO crud_test VALUES (2, 'to_delete')").unwrap();
        db.execute("INSERT INTO crud_test VALUES (3, 'unchanged')").unwrap();

        db.execute("UPDATE crud_test SET status = 'updated' WHERE id = 1")
            .unwrap();
        db.execute("DELETE FROM crud_test WHERE id = 2").unwrap();

        drop(db);
    }

    // Phase 2: Verify final state after recovery
    {
        let db = open_db(&db_path);
        let rows = db.query("SELECT id, status FROM crud_test", &[]).unwrap();

        // Should have 2 rows (id=1 updated, id=2 deleted, id=3 unchanged)
        assert_eq!(rows.len(), 2, "Should have 2 rows after recovery (1 deleted)");
    }
}

// ---------------------------------------------------------------------------
// Row-id counter reseed after a HARD crash (skips EmbeddedDatabase::drop)
// ---------------------------------------------------------------------------
//
// The 4 tests above use `drop(db)` to "simulate a crash". That is the CLEAN
// shutdown path: `Drop for EmbeddedDatabase` (src/lib.rs) unconditionally calls
// `flush_all_row_counters()` (which corrects the durable `counter:{table}` key)
// AND writes the R4.2 index snapshots. On reopen the counter is therefore
// already correct and the snapshot fast path is taken — so those tests can
// NEVER observe the stale-counter bug this section targets. They remain valid
// clean-shutdown recovery tests; they are just misleadingly named.
//
// The bug (silent row overwrite): the fast INSERT path allocates row ids from a
// volatile in-memory `AtomicU64` and only re-persists the durable counter every
// 64 rows. A table with < 64 inserts since its last %64 boundary has a STALE
// (too-low) durable counter. If the process dies WITHOUT running `Drop` — a
// hard crash (`kill -9`, SIGSEGV, power loss) — `flush_all_row_counters()` never
// runs, so on reopen `load_counters` seeds the in-memory counter from that stale
// value. The next INSERT then reuses an already-in-use row id and silently
// OVERWRITES the pre-existing `data:{table}:{row_id}` row. The fix reseeds the
// counter from the max row id actually present, inside the scan-fallback path of
// `Catalog::rebuild_all_indexes` (which runs precisely on a crash reopen — a
// valid index snapshot exists iff the last shutdown was clean).
//
// Why these tests use a CHILD PROCESS that calls `std::process::exit(0)` and NOT
// `std::mem::forget(db)` in-process (do NOT "simplify" this — neither `drop()`
// nor `mem::forget()` can reproduce the bug):
//   * `drop(db)` flushes the counter (clean path) — bug hidden.
//   * The bug requires `Drop` to be SKIPPED entirely (the counter flush at
//     src/lib.rs is unconditional, so any lock-releasing clean drop corrects it).
//   * `std::mem::forget(db)` skips `Drop`, BUT it leaks the RocksDB handle, so
//     its exclusive directory LOCK is never released. RocksDB tracks locked
//     paths PER PROCESS and rejects re-locking a still-held path ("lock hold by
//     current process"), so a same-path reopen IN THE SAME PROCESS would fail to
//     open — the test could never reach its assertions.
//   * A child process that calls `std::process::exit(0)` skips all Rust
//     destructors (counter stays stale — the crash condition) AND the OS
//     releases the file lock when the child dies (so the parent can reopen the
//     same path). Committed row data survives because the disk-backed write path
//     uses RocksDB's WAL (enabled, per-put `write()`), whose bytes are already
//     handed to the OS and are replayed on reopen.

/// Env var set by the parent to route a re-exec of this test binary into its
/// "crash child" role; its value is the data-dir path the child must populate.
const CRASH_CHILD_ENV: &str = "HELIOS_CRASH_CHILD_DB_PATH";

/// Re-exec THIS test binary, filtered to `test_name`, so its crash-child branch
/// runs: it opens the DB at `db_path`, applies the pre-crash SQL, and dies via
/// `std::process::exit(0)` (no `Drop`, lock released on process death). Blocks
/// until the child exits and asserts it exited cleanly.
fn crash_via_child(test_name: &str, db_path: &Path) {
    let status = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CRASH_CHILD_ENV, db_path)
        .status()
        .expect("failed to spawn crash-child process");
    assert!(
        status.success(),
        "crash-child process for '{}' did not exit cleanly (status: {:?})",
        test_name,
        status
    );
}

#[test]
fn test_crash_recovery_row_counter_reseed_after_uncommitted_flush() {
    // CHILD role: build the pre-crash state, then die WITHOUT running Drop so
    // the durable row counter is left stale (< 5). Insert exactly 5 rows via
    // normal SQL (< 64, so the fast INSERT path never hits its `row_id % 64`
    // durable-counter flush).
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_db(Path::new(&path));
        db.execute("CREATE TABLE reseed_test (id INT, name TEXT)").unwrap();
        for i in 1..=5 {
            db.execute(&format!("INSERT INTO reseed_test VALUES ({}, 'name_{}')", i, i))
                .unwrap();
        }
        // Hard crash: exit WITHOUT running Drop (counter stays stale); the OS
        // releases the RocksDB lock when this process dies. drop(db)/mem::forget
        // would NOT reproduce the bug (see module comment above).
        std::process::exit(0);
    }

    // PARENT role.
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();
    crash_via_child(
        "test_crash_recovery_row_counter_reseed_after_uncommitted_flush",
        &db_path,
    );

    // Reopen: rebuild_all_indexes' scan-fallback path must reseed the row-id
    // counter from the max row id actually present so the next INSERT does not
    // reuse an in-use row id and silently overwrite an existing row.
    {
        let db = open_db(&db_path);

        // Original data must be intact — 5 rows, all values present.
        let rows = db.query("SELECT id, name FROM reseed_test", &[]).unwrap();
        assert_eq!(rows.len(), 5, "All 5 original rows should survive crash recovery");
        let name3 = db.query("SELECT name FROM reseed_test WHERE id = 3", &[]).unwrap();
        assert_eq!(name3.len(), 1, "Row id=3 should still be present after recovery");

        // The critical assertion: inserting one more row must ADD a row, not
        // overwrite an existing one via a reused row id. Pre-fix, the durable
        // counter is stale (< 5) so this insert reuses row_id 1, keeping the
        // count at 5 and corrupting the first-inserted row.
        db.execute("INSERT INTO reseed_test VALUES (6, 'name_6')").unwrap();

        let rows_after = db.query("SELECT id, name FROM reseed_test", &[]).unwrap();
        assert_eq!(
            rows_after.len(),
            6,
            "After the 6th insert there must be 6 rows (5 would mean the insert \
             silently overwrote an existing row via a reused row id)"
        );

        // Re-verify every original row is still present and correct — a reused
        // row id would have clobbered one of these.
        for i in 1..=5 {
            let r = db
                .query(&format!("SELECT name FROM reseed_test WHERE id = {}", i), &[])
                .unwrap();
            assert_eq!(
                r.len(),
                1,
                "Original row id={} must still be present and uncorrupted after the 6th insert",
                i
            );
        }
        let r6 = db.query("SELECT name FROM reseed_test WHERE id = 6", &[]).unwrap();
        assert_eq!(r6.len(), 1, "Newly inserted row id=6 must be present");

        drop(db);
    }
}

#[test]
fn test_crash_recovery_row_counter_reseed_empty_table() {
    // CHILD role: create a table but insert ZERO rows, then hard-crash. This
    // exercises the empty-table path of the fix (max_row_id stays None -> no
    // reseed call), which must be a graceful no-op on reopen.
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_db(Path::new(&path));
        db.execute("CREATE TABLE empty_reseed (id INT, name TEXT)").unwrap();
        // Hard crash without Drop; drop()/mem::forget would not reproduce this.
        std::process::exit(0);
    }

    // PARENT role.
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();
    crash_via_child("test_crash_recovery_row_counter_reseed_empty_table", &db_path);

    // Reopen (empty-table reseed must be a no-op, no panic), then insert one
    // row and confirm it lands correctly.
    {
        let db = open_db(&db_path);

        let rows = db.query("SELECT id, name FROM empty_reseed", &[]).unwrap();
        assert_eq!(rows.len(), 0, "Table should be empty after crash recovery");

        db.execute("INSERT INTO empty_reseed VALUES (1, 'first')").unwrap();

        let rows_after = db.query("SELECT id, name FROM empty_reseed", &[]).unwrap();
        assert_eq!(rows_after.len(), 1, "Should have exactly 1 row after first insert");
        let r = db.query("SELECT name FROM empty_reseed WHERE id = 1", &[]).unwrap();
        assert_eq!(r.len(), 1, "The inserted row should be present with correct data");

        drop(db);
    }
}

#[test]
fn test_crash_recovery_row_counter_reseed_multiple_tables() {
    // CHILD role: two tables with DIFFERENT (< 64) row counts, then hard-crash
    // so both durable counters are left stale independently.
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_db(Path::new(&path));
        db.execute("CREATE TABLE reseed_a (id INT, name TEXT)").unwrap();
        db.execute("CREATE TABLE reseed_b (id INT, name TEXT)").unwrap();
        for i in 1..=3 {
            db.execute(&format!("INSERT INTO reseed_a VALUES ({}, 'a_{}')", i, i))
                .unwrap();
        }
        for i in 1..=7 {
            db.execute(&format!("INSERT INTO reseed_b VALUES ({}, 'b_{}')", i, i))
                .unwrap();
        }
        // Hard crash without Drop; drop()/mem::forget would not reproduce this.
        std::process::exit(0);
    }

    // PARENT role.
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();
    crash_via_child("test_crash_recovery_row_counter_reseed_multiple_tables", &db_path);

    // Reopen and insert one more row into EACH table. Both counters must have
    // been reseeded independently (row ids are per-table), so each insert adds
    // a row rather than overwriting.
    {
        let db = open_db(&db_path);

        assert_eq!(
            db.query("SELECT id FROM reseed_a", &[]).unwrap().len(),
            3,
            "Table A should still have 3 rows after recovery"
        );
        assert_eq!(
            db.query("SELECT id FROM reseed_b", &[]).unwrap().len(),
            7,
            "Table B should still have 7 rows after recovery"
        );

        db.execute("INSERT INTO reseed_a VALUES (4, 'a_4')").unwrap();
        db.execute("INSERT INTO reseed_b VALUES (8, 'b_8')").unwrap();

        assert_eq!(
            db.query("SELECT id FROM reseed_a", &[]).unwrap().len(),
            4,
            "Table A must have 4 rows after its 4th insert (no reused row id)"
        );
        assert_eq!(
            db.query("SELECT id FROM reseed_b", &[]).unwrap().len(),
            8,
            "Table B must have 8 rows after its 8th insert (no reused row id)"
        );

        // Explicitly confirm no cross-table bleed: every logical row inserted
        // into each table is present, i.e. the two tables did not share counter
        // state (which would have caused one table's insert to collide).
        for i in 1..=4 {
            let r = db
                .query(&format!("SELECT name FROM reseed_a WHERE id = {}", i), &[])
                .unwrap();
            assert_eq!(r.len(), 1, "reseed_a row id={} must be present", i);
        }
        for i in 1..=8 {
            let r = db
                .query(&format!("SELECT name FROM reseed_b WHERE id = {}", i), &[])
                .unwrap();
            assert_eq!(r.len(), 1, "reseed_b row id={} must be present", i);
        }

        drop(db);
    }
}
