//! Sprinter e21903322db7 (spec R1 §4): the residuals of
//! `REPLICATION_4_41_1_DESIGN.md` §7-8, re-triaged under physical replication.
//! A standby no longer reads the logical WAL at all; what is left of it is the
//! PRIMARY's own redo window, replayed at open after a crash
//! (`StorageEngine::recover_wal_at_open`). These tests crash a primary-shaped
//! store for real — a child process runs the workload and exits without
//! running a single `Drop` — and reopen it, so the logical WAL above the
//! checkpoint is replayed exactly as after a `kill -9`. Where it matters the
//! reopen is made under each destructive-replay policy (`refuse`, the
//! default, `warn` and `apply`), each on its own crashed store.
//!
//! * `the_residual_logging_shapes_replay_to_the_committed_state` proves the
//!   residual shapes harmless for the primary's own recovery: an explicit
//!   transaction (logged at commit), a rolled-back one, DDL inside an
//!   explicit `BEGIN` (no capture frame: its entry is in the catalog change's
//!   own write group), autocommit DDL (the same, inside the statement's
//!   frame), autocommit DML logged per statement (the HA primary's shape),
//!   TRUNCATE. After the replay every table holds exactly its committed rows,
//!   and the replay changed no `data:` key or value the crashed store held.
//! * `a_dropped_and_recreated_table_keeps_only_its_new_rows_under_every_policy`
//!   and `a_destructive_entry_fences_the_entries_of_the_incarnation_it_ended`
//!   are the defect the re-triage found: the entries of a table incarnation a
//!   DROP, RENAME or TRUNCATE ended were re-applied to whatever bore the name
//!   — around a refused destructive entry (`refuse`), and under `warn` /
//!   `apply` also because the replay did not write its batched rows before it
//!   ran a DDL entry. Red before the fix: `x` exists, `d` holds `[2, 3, 10]`
//!   (every policy), `s` holds `[2, 9]`.
//! * `begin_truncate_rollback_keeps_the_rows_under_every_policy`: a
//!   transactional TRUNCATE that rolled back left its entry (it was appended
//!   at once), and `warn` / `apply` truncated the table the rollback kept.
//! * (`internal-tests`) a RENAME, a DROP and a CREATE inside an explicit
//!   `BEGIN`, each crashed right after its catalog write (the failpoint
//!   `HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE`): the entry is in that very
//!   write, so the old table never comes back (red when the entry was
//!   appended on its own, after the write).
//!
//! The store is checkpointed by a clean first session (a store with no
//! checkpoint is adopted without any replay), and the periodic checkpoint is
//! off, so the whole crashed session is above the checkpoint — the window a
//! crash leaves since the last periodic tick (default 1000 entries / 30 s).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::DestructiveDdlReplay;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::{Options, DB};
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;

/// Set by the parent on its re-exec of this test binary: the data directory
/// the crash child populates.
const CRASH_CHILD_ENV: &str = "HELIOS_R1_REPLAY_CRASH_CHILD_DB_PATH";
/// The engine's `internal-tests` failpoint: end the process right after the
/// catalog write of one DDL (`<create|drop|rename>:<table>`).
#[cfg(feature = "internal-tests")]
const EXIT_AFTER_DDL_WRITE_ENV: &str = "HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE";

const POLICIES: [DestructiveDdlReplay; 3] = [
    DestructiveDdlReplay::Refuse,
    DestructiveDdlReplay::Warn,
    DestructiveDdlReplay::Apply,
];

/// The primary's shape: persistent, logical WAL on, autocommit DML logged per
/// statement (as `fast_dml_requires_logical_wal` makes it on an HA primary),
/// no periodic checkpoint, the destructive-replay `policy`.
fn config_for(dir: &Path, policy: DestructiveDdlReplay) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(dir.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_on_close = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_replay_destructive_ddl = policy;
    config
}

/// Retried: a handle closed a moment ago (the seed's, in the parent) may
/// release the directory lock from its background threads.
fn open_db(dir: &Path, policy: DestructiveDdlReplay) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config_for(dir, policy)) {
            Ok(db) => return db,
            Err(error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("open database: {last:?}");
}

/// The crash child's open (its policy never matters: it replays nothing).
fn open_child(path: &str) -> EmbeddedDatabase {
    open_db(Path::new(path), DestructiveDdlReplay::Refuse)
}

/// A first, CLEAN session: the close stamps `wal:checkpoint`, so the crashed
/// session's entries are replayed at the next open (a store that never had a
/// checkpoint is adopted without replay — no hazard, and no test).
fn seed(dir: &Path) {
    let db = open_db(dir, DestructiveDdlReplay::Refuse);
    db.execute("CREATE TABLE seed (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO seed VALUES (1, 'seed')").unwrap();
    drop(db);
}

/// Re-exec THIS test binary, filtered to `test_name`, as the crash child: it
/// runs the workload on `dir` and dies — by `std::process::exit(0)` at the
/// end of its workload, or inside it at the failpoint `env` arms — with no
/// `Drop` and no close-time checkpoint; the OS releases the RocksDB lock.
fn crash_child(test_name: &str, dir: &Path, env: &[(&str, &str)]) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CRASH_CHILD_ENV, dir);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command.status().unwrap();
    assert!(status.success(), "the crash child of {test_name} failed: {status:?}");
}

/// A fresh store: seeded clean, then crashed by `test_name`'s child.
fn crashed_store(test_name: &str, env: &[(&str, &str)]) -> TempDir {
    let temp = TempDir::new().unwrap();
    seed(temp.path());
    crash_child(test_name, temp.path(), env);
    assert!(
        retained_above_checkpoint(temp.path()) > 0,
        "vacuity: the crashed session must have left entries above the checkpoint"
    );
    temp
}

/// Every `data:` key and value of the store at `dir`, read-only (the engine
/// may hold it).
fn data_keys(dir: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let db = DB::open_for_read_only(&Options::default(), dir, false).unwrap();
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let mut keys = BTreeMap::new();
    for item in db.iterator_opt(
        rocksdb::IteratorMode::From(b"data:".as_slice(), rocksdb::Direction::Forward),
        read,
    ) {
        let (key, value) = item.unwrap();
        if !key.starts_with(b"data:") {
            break;
        }
        keys.insert(key.to_vec(), value.to_vec());
    }
    keys
}

/// The logical WAL entries the crashed store retains above its checkpoint.
fn retained_above_checkpoint(dir: &Path) -> usize {
    let db = DB::open_for_read_only(&Options::default(), dir, false).unwrap();
    let checkpoint = db
        .get(b"wal:checkpoint")
        .unwrap()
        .map(|bytes| u64::from_le_bytes(bytes.as_slice().try_into().unwrap()))
        .expect("vacuity: the seed session stamped the checkpoint");
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let mut above = 0;
    for item in db.iterator_opt(
        rocksdb::IteratorMode::From(b"wal:entries:".as_slice(), rocksdb::Direction::Forward),
        read,
    ) {
        let (key, _) = item.unwrap();
        let Some(lsn) = key.strip_prefix(b"wal:entries:".as_slice()) else {
            break;
        };
        let lsn: u64 = std::str::from_utf8(lsn).unwrap().parse().unwrap();
        if lsn > checkpoint {
            above += 1;
        }
    }
    above
}

/// `SELECT id … ORDER BY id` of `table`; `None` when the table does not exist.
fn ids(db: &EmbeddedDatabase, table: &str) -> Option<Vec<i64>> {
    let rows = db.query(&format!("SELECT id FROM {table} ORDER BY id"), &[]).ok()?;
    Some(
        rows.iter()
            .map(|row| match &row.values[0] {
                Value::Int2(v) => i64::from(*v),
                Value::Int4(v) => i64::from(*v),
                Value::Int8(v) => *v,
                other => panic!("{table}: an id of {other:?}"),
            })
            .collect(),
    )
}

fn text(db: &EmbeddedDatabase, sql: &str) -> Option<String> {
    let rows = db.query(sql, &[]).ok()?;
    rows.first().map(|row| match &row.values[0] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

#[test]
fn the_residual_logging_shapes_replay_to_the_committed_state() {
    const NAME: &str = "the_residual_logging_shapes_replay_to_the_committed_state";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE a (id INT PRIMARY KEY, v TEXT)").unwrap();
        // An explicit transaction: logged at COMMIT, staged into its batch.
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO a VALUES (1, 'one')").unwrap();
        db.execute("INSERT INTO a VALUES (2, 'two')").unwrap();
        db.execute("UPDATE a SET v = 'one, updated' WHERE id = 1").unwrap();
        db.execute("DELETE FROM a WHERE id = 2").unwrap();
        db.execute("COMMIT").unwrap();
        // A rolled-back one: nothing logged.
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO a VALUES (3, 'rolled back')").unwrap();
        db.execute("ROLLBACK").unwrap();
        // DDL inside an explicit BEGIN: no capture frame; its entry is in the
        // catalog change's own write group.
        db.execute("BEGIN").unwrap();
        db.execute("CREATE TABLE b (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO b VALUES (1, 'in a transaction')").unwrap();
        db.execute("COMMIT").unwrap();
        // Autocommit DDL: the same, inside the statement's capture frame.
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("ALTER TABLE c ADD COLUMN w INT").unwrap();
        db.execute("INSERT INTO c VALUES (1, 'c', 7)").unwrap();
        // Autocommit DML, logged per statement.
        db.execute("INSERT INTO a VALUES (4, 'autocommit')").unwrap();
        db.execute("UPDATE a SET v = 'autocommit, updated' WHERE id = 4")
            .unwrap();
        // TRUNCATE between logged rows (both families log it).
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO r VALUES (1, 'gone'), (2, 'gone')").unwrap();
        db.execute_params("TRUNCATE TABLE r", &[]).unwrap();
        db.execute("INSERT INTO r VALUES (9, 'after the truncate')").unwrap();
        std::process::exit(0);
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[]);
        let dir = temp.path();
        let crashed = data_keys(dir);

        let db = open_db(dir, policy);
        assert_eq!(ids(&db, "seed"), Some(vec![1]), "{policy:?}");
        assert_eq!(ids(&db, "a"), Some(vec![1, 4]), "{policy:?}");
        assert_eq!(
            text(&db, "SELECT v FROM a WHERE id = 1").as_deref(),
            Some("one, updated")
        );
        assert_eq!(
            text(&db, "SELECT v FROM a WHERE id = 4").as_deref(),
            Some("autocommit, updated")
        );
        assert_eq!(ids(&db, "b"), Some(vec![1]), "{policy:?}");
        assert_eq!(ids(&db, "c"), Some(vec![1]), "{policy:?}");
        assert_eq!(text(&db, "SELECT w FROM c WHERE id = 1").as_deref(), Some("7"));
        assert_eq!(ids(&db, "r"), Some(vec![9]), "{policy:?}");
        let replayed = data_keys(dir);
        assert_eq!(
            replayed.keys().collect::<Vec<_>>(),
            crashed.keys().collect::<Vec<_>>(),
            "{policy:?}: the replay added or removed rows the crashed store held"
        );
        let changed: Vec<String> = crashed
            .iter()
            .filter(|(key, value)| replayed.get(*key) != Some(*value))
            .map(|(key, _)| String::from_utf8_lossy(key).into_owned())
            .collect();
        assert!(changed.is_empty(), "{policy:?}: the replay rewrote rows: {changed:?}");
    }
}

/// Spec R1 §4: drop and re-create under the same name, rows logged on both
/// sides. Under `warn` / `apply` the replay applies the DROP, and must not
/// write the old incarnation's batched rows after it.
#[test]
fn a_dropped_and_recreated_table_keeps_only_its_new_rows_under_every_policy() {
    const NAME: &str = "a_dropped_and_recreated_table_keeps_only_its_new_rows_under_every_policy";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO d VALUES (1, 'old 1'), (2, 'old 2'), (3, 'old 3')")
            .unwrap();
        db.execute("DROP TABLE d").unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO d VALUES (10, 'new')").unwrap();
        std::process::exit(0);
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[]);
        let dir = temp.path();
        let crashed = data_keys(dir);
        let db = open_db(dir, policy);
        assert_eq!(
            ids(&db, "d"),
            Some(vec![10]),
            "*** {policy:?}: the replay wrote the dropped incarnation's rows into the re-created table ***"
        );
        assert_eq!(text(&db, "SELECT v FROM d WHERE id = 10").as_deref(), Some("new"));
        assert_eq!(ids(&db, "seed"), Some(vec![1]));
        assert_eq!(
            data_keys(dir),
            crashed,
            "{policy:?}: the replay changed rows the crashed store held"
        );
    }
}

/// The default policy: a dropped table, and a renamed one whose source name
/// was re-created, keep the state the crashed store held. (Under `warn` /
/// `apply` a replayed rename consumes a re-created source table by design —
/// GH#35's escape hatch — so only `refuse` is asserted here.)
#[test]
fn a_destructive_entry_fences_the_entries_of_the_incarnation_it_ended() {
    const NAME: &str = "a_destructive_entry_fences_the_entries_of_the_incarnation_it_ended";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        // Dropped for good.
        db.execute("CREATE TABLE x (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO x VALUES (1, 'dropped')").unwrap();
        db.execute("DROP TABLE x").unwrap();
        // Renamed away, and the source name re-created.
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO s VALUES (1, 's 1'), (2, 's 2')").unwrap();
        db.execute("ALTER TABLE s RENAME TO s2").unwrap();
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO s VALUES (9, 'new s')").unwrap();
        std::process::exit(0);
    }

    let temp = crashed_store(NAME, &[]);
    let dir = temp.path();
    let crashed = data_keys(dir);
    let db = open_db(dir, DestructiveDdlReplay::Refuse);
    assert_eq!(
        ids(&db, "x"),
        None,
        "*** the replay re-created a dropped table (its CreateTable replayed, its DropTable refused) ***"
    );
    assert_eq!(ids(&db, "s2"), Some(vec![1, 2]));
    assert_eq!(
        ids(&db, "s"),
        Some(vec![9]),
        "*** the replay wrote the renamed-away incarnation's rows into the re-created source table ***"
    );
    assert_eq!(ids(&db, "seed"), Some(vec![1]));
    assert_eq!(
        data_keys(dir),
        crashed,
        "the replay changed rows the crashed store held"
    );
}

/// Spec R1 §4: a transactional TRUNCATE that rolled back leaves no entry —
/// its entry belongs to the commit — so no policy truncates the rows the
/// rollback kept, nor the one committed after it.
#[test]
fn begin_truncate_rollback_keeps_the_rows_under_every_policy() {
    const NAME: &str = "begin_truncate_rollback_keeps_the_rows_under_every_policy";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO r VALUES (1, 'kept'), (2, 'kept')").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("TRUNCATE r").unwrap();
        db.execute("ROLLBACK").unwrap();
        db.execute("INSERT INTO r VALUES (3, 'after the rollback')").unwrap();
        std::process::exit(0);
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[]);
        let db = open_db(temp.path(), policy);
        assert_eq!(
            ids(&db, "r"),
            Some(vec![1, 2, 3]),
            "*** {policy:?}: a rolled-back TRUNCATE was replayed ***"
        );
    }
}

/// Spec R1 §4: a RENAME crashed right after its catalog write. Its entry is
/// in that write, so the source's CREATE and inserts above the checkpoint
/// are fenced and the old name never comes back, under every policy.
#[cfg(feature = "internal-tests")]
#[test]
fn a_rename_crashed_after_its_catalog_write_does_not_come_back() {
    const NAME: &str = "a_rename_crashed_after_its_catalog_write_does_not_come_back";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE keep (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO keep VALUES (1, 'k'), (2, 'k')").unwrap();
        db.execute("CREATE TABLE a (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO a VALUES (1, 'a 1'), (2, 'a 2')").unwrap();
        // Ends the process right after the rename's catalog write.
        let outcome = db.execute("ALTER TABLE a RENAME TO b");
        panic!("the failpoint did not end the process: {outcome:?}");
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[(EXIT_AFTER_DDL_WRITE_ENV, "rename:a")]);
        let db = open_db(temp.path(), policy);
        assert_eq!(
            ids(&db, "a"),
            None,
            "*** {policy:?}: the renamed-away table came back ***"
        );
        assert_eq!(ids(&db, "b"), Some(vec![1, 2]), "{policy:?}");
        assert_eq!(ids(&db, "keep"), Some(vec![1, 2]), "{policy:?}");
    }
}

/// Spec R1 §4: a DROP crashed right after its catalog write: the dropped
/// table never comes back, and the other table's rows survive, under every
/// policy.
#[cfg(feature = "internal-tests")]
#[test]
fn a_drop_crashed_after_its_catalog_write_does_not_come_back() {
    const NAME: &str = "a_drop_crashed_after_its_catalog_write_does_not_come_back";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE keep (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO keep VALUES (1, 'k'), (2, 'k')").unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO d VALUES (1, 'd 1'), (2, 'd 2')").unwrap();
        // Ends the process right after the drop's catalog write.
        let outcome = db.execute("DROP TABLE d");
        panic!("the failpoint did not end the process: {outcome:?}");
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[(EXIT_AFTER_DDL_WRITE_ENV, "drop:d")]);
        let db = open_db(temp.path(), policy);
        assert_eq!(ids(&db, "d"), None, "*** {policy:?}: the dropped table came back ***");
        assert_eq!(ids(&db, "keep"), Some(vec![1, 2]), "{policy:?}");
    }
}

/// Spec R1 §4: DDL inside an explicit `BEGIN` — no capture frame — crashed
/// right after its catalog write: the table it created is there (DDL is not
/// transactional) and empty, the transaction's uncommitted row is not, the
/// committed rows are, under every policy.
#[cfg(feature = "internal-tests")]
#[test]
fn ddl_inside_an_explicit_begin_crashed_after_its_catalog_write() {
    const NAME: &str = "ddl_inside_an_explicit_begin_crashed_after_its_catalog_write";
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open_child(&path);
        db.execute("CREATE TABLE keep (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO keep VALUES (1, 'k'), (2, 'k')").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO keep VALUES (3, 'uncommitted')").unwrap();
        // Ends the process right after the create's catalog write.
        let outcome = db.execute("CREATE TABLE b (id INT PRIMARY KEY, v TEXT)");
        panic!("the failpoint did not end the process: {outcome:?}");
    }

    for policy in POLICIES {
        let temp = crashed_store(NAME, &[(EXIT_AFTER_DDL_WRITE_ENV, "create:b")]);
        let db = open_db(temp.path(), policy);
        assert_eq!(ids(&db, "b"), Some(Vec::new()), "{policy:?}");
        assert_eq!(ids(&db, "keep"), Some(vec![1, 2]), "{policy:?}");
    }
}
