//! WAL replay must never re-execute history.
//!
//! # The bug these tests pin
//!
//! `WriteAheadLog::replay()` returned EVERY retained `wal:entries:` record and
//! `StorageEngine::open` applied all of them on every open. There was no
//! checkpoint filter — the function's own doc comment said so. The intended
//! design was "replay once, then truncate", which breaks down completely for a
//! store written by an older build that never truncated: the first open by a
//! current binary replays the ENTIRE retained history, re-executing historical
//! DDL against tables that legitimately exist now.
//!
//! Reproduced twice on copies of a real production store (3.58.1, 38 MB). Its
//! retained log held 2058 entries, of which:
//!
//! ```text
//!   lsn=219996 CreateTable organizations_new
//!   lsn=219997 CreateTable organizations_new
//!   lsn=220000 DropTable   organizations
//! ```
//!
//! with NO `RenameTable` entry anywhere — the rename that completed that
//! maintenance was never logged, because that WAL op did not exist in the older
//! engine. A whole-keyspace diff across ONE open showed
//! `meta:table:organizations` and `data:organizations:5` (a real customer row)
//! REMOVED, and `meta:table:organizations_new` (the older schema) ADDED. The
//! live table and its row were destroyed, silently, by opening the store.
//!
//! # What is asserted here
//!
//! Every assertion below is on DATA — a table's existence and its rows — never
//! on a log line. A test that passed by reading a log message would not have
//! caught the original bug either.
//!
//! # How the "retained entries, no checkpoint" state is reached
//!
//! Naturally, and without any special-casing: the first open of a store finds
//! an empty log, so nothing is replayed and nothing is truncated, so no
//! `wal:checkpoint` key is ever written. Entries appended during that session
//! are still there at the next open, and that next open is the one that used to
//! re-execute them. The crafted-entry tests below reach the same state by
//! appending through the ordinary `WriteAheadLog` API against a closed store —
//! chosen over driving DDL through SQL because it pins the EXACT operation from
//! the production evidence (`DropTable` for a table that exists now) instead of
//! whichever subset of DDL the current engine happens to log.

use heliosdb_nano::{
    storage::{WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase, Value,
};
use rocksdb::{Options, DB};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// `WriteAheadLog::CHECKPOINT_KEY`, restated so the tests fail loudly if the
/// on-disk name ever changes without the tests being revisited.
const CHECKPOINT_KEY: &[u8] = b"wal:checkpoint";
const ENTRY_PREFIX: &[u8] = b"wal:entries:";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn config_for(dir: &Path, logical_wal_per_statement: bool) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = logical_wal_per_statement;
    // These tests hand-construct the pre-fix on-disk states (retained entries
    // above an absent or planted checkpoint) by writing to a CLOSED store, so
    // the GH#35 close-time checkpoint — which would reclaim exactly those
    // entries — and the periodic triggers are disabled here. The close-time
    // behavior itself is covered by tests/gh_issue_35.rs.
    c.storage.wal_checkpoint_on_close = false;
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c
}

fn open_db(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, false)).expect("open database")
}

fn open_db_logging_dml(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config_for(dir, true)).expect("open database")
}

/// Raw RocksDB handle on a CLOSED store.
///
/// The 5-byte fixed prefix extractor matches the one `StorageEngine::open`
/// configures, so the handle iterates the store exactly the way the engine
/// does. Retried briefly because the previous handle's background threads
/// release the RocksDB directory lock asynchronously.
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
    panic!("raw RocksDB open failed: {:?}", last_err);
}

fn retained_entry_count(db: &DB) -> usize {
    db.prefix_iterator(ENTRY_PREFIX)
        .filter_map(|item| item.ok())
        .take_while(|(key, _)| key.starts_with(ENTRY_PREFIX))
        .count()
}

fn checkpoint_of(db: &DB) -> Option<u64> {
    db.get(CHECKPOINT_KEY)
        .expect("read checkpoint")
        .map(|bytes| u64::from_le_bytes(bytes.as_slice().try_into().expect("checkpoint is 8 bytes")))
}

fn text_column(db: &EmbeddedDatabase, sql: &str, column: usize) -> Vec<String> {
    let rows = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"));
    let mut out: Vec<String> = rows
        .iter()
        .map(|tuple| match &tuple.values[column] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// 1. THE REGRESSION. A stale DropTable must not destroy a live table.
// ---------------------------------------------------------------------------

/// This is the production shape, reduced: a `DropTable` for `organizations`
/// sitting in a retained, never-checkpointed log, while `organizations` exists
/// and holds a customer row.
///
/// On the unfixed tree the reopen replays that entry, `Catalog::drop_table`
/// runs, and both the table and its row are gone — the query below fails, or
/// returns nothing. On the fixed tree the absent `wal:checkpoint` classifies
/// the log as already-applied history and it is adopted rather than replayed.
#[test]
fn a_stale_drop_table_in_an_uncheckpointed_log_must_not_destroy_the_live_table() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE organizations (id INT PRIMARY KEY, name TEXT)")
            .expect("create table");
        db.execute("INSERT INTO organizations VALUES (5, 'Acme Customer')")
            .expect("insert");
        assert_eq!(
            text_column(&db, "SELECT id, name FROM organizations", 1),
            vec!["Acme Customer".to_string()],
            "sanity: the row must exist before the store is closed"
        );
    }

    // Plant the historical entry, exactly as an older build would have left it.
    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        wal.append(WalOperation::DropTable {
            table: "organizations".to_string(),
        })
        .expect("append historical DropTable");

        // VACUITY GUARDS. Without these, a fix that simply stopped writing WAL
        // entries — or a harness that never got one on disk — would make the
        // assertion below pass while proving nothing.
        assert!(
            retained_entry_count(&raw) > 0,
            "the store must actually retain the DropTable entry, else the reopen below has \
             nothing to (wrongly) replay"
        );
        assert_eq!(
            checkpoint_of(&raw),
            None,
            "the store must be in the un-checkpointed state this bug is about: a first open \
             finds an empty log, replays nothing, and therefore truncates nothing"
        );
        assert!(
            raw.get(b"meta:table:organizations").expect("read schema").is_some(),
            "sanity: the table's schema record must be present before the reopen"
        );
    }

    // The open under test.
    {
        let db = open_db(dir);
        assert_eq!(
            text_column(&db, "SELECT id, name FROM organizations", 1),
            vec!["Acme Customer".to_string()],
            "opening the store replayed a historical DropTable and destroyed the live table"
        );
    }
}

/// The same shape for `Truncate`, the other replayed operation that destroys
/// rows without destroying the table — so a fix that special-cased `DropTable`
/// alone would not pass.
#[test]
fn a_stale_truncate_in_an_uncheckpointed_log_must_not_empty_the_live_table() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE ledger (id INT PRIMARY KEY, memo TEXT)")
            .expect("create table");
        db.execute("INSERT INTO ledger VALUES (1, 'entry-one')")
            .expect("insert 1");
        db.execute("INSERT INTO ledger VALUES (2, 'entry-two')")
            .expect("insert 2");
    }

    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        wal.append(WalOperation::Truncate {
            table: "ledger".to_string(),
        })
        .expect("append historical Truncate");
        assert!(retained_entry_count(&raw) > 0, "vacuity: the entry must be retained");
        assert_eq!(checkpoint_of(&raw), None, "vacuity: the store must be un-checkpointed");
    }

    {
        let db = open_db(dir);
        assert_eq!(
            text_column(&db, "SELECT id, memo FROM ledger ORDER BY id", 1),
            vec!["entry-one".to_string(), "entry-two".to_string()],
            "opening the store replayed a historical Truncate and emptied the live table"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The upgrade leaves the store in the state the next open expects.
// ---------------------------------------------------------------------------

/// Adoption is not just "skip replay": it must RECORD what it adopted, or every
/// subsequent open would face the same undecidable log and the store could
/// never accumulate a real redo window.
#[test]
fn the_legacy_upgrade_records_a_checkpoint_and_reclaims_the_log() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO t VALUES (1, 'kept')").expect("insert");
    }

    let planted_lsn = {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        let lsn = wal
            .append(WalOperation::DropTable { table: "t".to_string() })
            .expect("append");
        assert_eq!(checkpoint_of(&raw), None, "vacuity: un-checkpointed before the upgrade");
        lsn
    };

    {
        let _db = open_db(dir);
    }

    let raw = open_raw(dir);
    let checkpoint = checkpoint_of(&raw).expect("the upgrade must record a checkpoint");
    assert!(
        checkpoint >= planted_lsn,
        "the adopted checkpoint ({checkpoint}) must cover every entry that was retained \
         (highest was {planted_lsn}), or the next open would replay them after all"
    );
    assert_eq!(
        retained_entry_count(&raw),
        0,
        "the adopted entries must also be reclaimed — they can never be needed again"
    );
}

/// Opening the same store repeatedly must converge, not degrade. The original
/// bug destroyed a little more on each open; this pins the opposite.
#[test]
fn reopening_repeatedly_is_stable() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE orgs (id INT PRIMARY KEY, name TEXT)")
            .expect("create");
        db.execute("INSERT INTO orgs VALUES (1, 'first')").expect("insert 1");
        db.execute("INSERT INTO orgs VALUES (2, 'second')").expect("insert 2");
    }

    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        wal.append(WalOperation::DropTable {
            table: "orgs".to_string(),
        })
        .expect("append");
        assert_eq!(checkpoint_of(&raw), None, "vacuity: un-checkpointed");
    }

    let expected = vec!["first".to_string(), "second".to_string()];
    for round in 1..=4 {
        let db = open_db(dir);
        assert_eq!(
            text_column(&db, "SELECT id, name FROM orgs ORDER BY id", 1),
            expected,
            "the rows must be unchanged on open #{round}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Recovery is still recovery. This is the pair that stops the fix from
//    being "disable replay".
// ---------------------------------------------------------------------------

/// Simulates the ONE window logical redo genuinely covers: the WAL entry
/// landed, the corresponding data write did not.
///
/// The data key is removed from RocksDB directly and the checkpoint is planted
/// just BELOW that entry's LSN, so the entry is unambiguously post-checkpoint.
/// Reopening must put the row back. If this test ever fails, the fix has turned
/// crash recovery into a no-op.
#[test]
fn an_entry_past_the_checkpoint_is_still_replayed() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    // `logical_wal_per_statement` so that autocommit DML lands in the logical
    // WAL — the default relies on the RocksDB write path instead, which would
    // leave nothing here to replay.
    {
        let db = open_db_logging_dml(dir);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO t VALUES (1, 'alpha')").expect("insert 1");
        db.execute("INSERT INTO t VALUES (2, 'beta')").expect("insert 2");
    }

    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        let entries = wal.replay().expect("read the retained log");

        // The last logged Insert for `t` — its `key` field IS the data key the
        // engine wrote, so no knowledge of the row encoding is needed here.
        let (lsn, data_key) = entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.operation {
                WalOperation::Insert { table, key, .. } if table == "t" => Some((entry.lsn, key.clone())),
                _ => None,
            })
            .expect("vacuity: the logical WAL must contain an Insert for `t`, else this test is empty");

        assert!(
            data_key.starts_with(b"data:"),
            "vacuity: the logged Insert must carry the real storage key (some paths log an empty \
             key, which would make the deletion below a no-op). Got: {:?}",
            String::from_utf8_lossy(&data_key)
        );
        assert!(
            raw.get(&data_key).expect("read data key").is_some(),
            "vacuity: the data key must exist before it is removed"
        );
        raw.delete(&data_key).expect("simulate the lost data write");
        assert!(
            raw.get(&data_key).expect("read data key").is_none(),
            "vacuity: the data key must really be gone, else the reopen proves nothing"
        );

        // Everything strictly before this entry is checkpointed; this entry is
        // not. (Planted directly: the filter must be correct for any
        // checkpoint/retained pair, and this is the pair that matters.)
        raw.put(CHECKPOINT_KEY, (lsn - 1).to_le_bytes())
            .expect("plant checkpoint");
    }

    {
        let db = open_db_logging_dml(dir);
        assert_eq!(
            text_column(&db, "SELECT id, v FROM t ORDER BY id", 1),
            vec!["alpha".to_string(), "beta".to_string()],
            "an entry past the checkpoint was NOT replayed — crash recovery has become a no-op"
        );
    }
}

/// The other half of the pair: the same store, the same missing row, but with
/// the checkpoint covering that entry. It must NOT come back — otherwise the
/// checkpoint filter is not filtering and the fix is cosmetic.
#[test]
fn an_entry_at_or_below_the_checkpoint_is_not_replayed() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let db = open_db_logging_dml(dir);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO t VALUES (1, 'alpha')").expect("insert 1");
        db.execute("INSERT INTO t VALUES (2, 'beta')").expect("insert 2");
    }

    {
        let raw = open_raw(dir);
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        let entries = wal.replay().expect("read the retained log");

        let (lsn, data_key) = entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.operation {
                WalOperation::Insert { table, key, .. } if table == "t" => Some((entry.lsn, key.clone())),
                _ => None,
            })
            .expect("vacuity: the logical WAL must contain an Insert for `t`");

        assert!(
            data_key.starts_with(b"data:"),
            "vacuity: the logged Insert must carry the real storage key"
        );
        assert!(
            raw.get(&data_key).expect("read data key").is_some(),
            "vacuity: the data key must exist before it is removed"
        );
        raw.delete(&data_key).expect("simulate the lost data write");

        let highest = entries.iter().map(|e| e.lsn).max().expect("non-empty log");
        assert!(highest >= lsn, "sanity: the checkpoint must cover the entry under test");
        raw.put(CHECKPOINT_KEY, highest.to_le_bytes())
            .expect("plant checkpoint");
    }

    {
        let db = open_db_logging_dml(dir);
        assert_eq!(
            text_column(&db, "SELECT id, v FROM t ORDER BY id", 1),
            vec!["alpha".to_string()],
            "a checkpointed entry was replayed anyway — the checkpoint filter is not filtering"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A fresh store is untouched.
// ---------------------------------------------------------------------------

/// An empty store must open, close and reopen with no recovery activity at all,
/// and must NOT be stamped with a checkpoint.
///
/// The second half is a deliberate design choice, pinned here so it cannot be
/// "tidied up" later: `wal:checkpoint = N` is a claim that entries up to `N`
/// are applied. Stamping it over an empty log is vacuously true, but it
/// reclassifies the store from "never checkpointed" (unknown provenance,
/// handled conservatively) to "checkpointed" (trusted, replayed) with no
/// evidence gathered — which would put the very first reopen of any new store
/// back on the destructive path.
#[test]
fn a_fresh_store_is_unaffected_and_is_not_stamped_with_a_checkpoint() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    {
        let _db = open_db(dir);
    }

    {
        let raw = open_raw(dir);
        assert_eq!(retained_entry_count(&raw), 0, "a fresh store retains no WAL entries");
        assert_eq!(
            checkpoint_of(&raw),
            None,
            "an empty log must not be stamped with a checkpoint"
        );
    }

    {
        let db = open_db(dir);
        db.execute("CREATE TABLE fresh (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO fresh VALUES (1, 'ok')").expect("insert");
    }

    {
        let db = open_db(dir);
        assert_eq!(
            text_column(&db, "SELECT id, v FROM fresh", 1),
            vec!["ok".to_string()],
            "an ordinary create/insert/close/reopen cycle must be unaffected"
        );
    }
}
