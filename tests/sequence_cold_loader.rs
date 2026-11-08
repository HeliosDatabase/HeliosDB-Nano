//! A cold sequence decoder must not resurrect old config after DDL eviction.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::sql::sequences::{
    debug_load_runtime_on, debug_next_after_cold_runtime_read, debug_next_before_ddl_owner,
};
use heliosdb_nano::storage::ddl_batch::debug_next_after_native_commit;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut c = Config::default();
    c.storage.path = Some(path.into());
    c.storage.memory_only = false;
    c.storage.time_travel_enabled = history;
    c.storage.version_gc_interval_secs = Some(0);
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c.storage.wal_checkpoint_on_close = false;
    c.storage.flush_on_close = false;
    let db = EmbeddedDatabase::with_config(c).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}
fn exec(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}
fn next(db: &EmbeddedDatabase, params: bool) -> i64 {
    let rows = if params {
        db.query_params("SELECT nextval('target')", &[])
    } else {
        db.query("SELECT nextval('target')", &[])
    }
    .unwrap();
    assert_eq!(rows.len(), 1);
    match rows[0].values[0] {
        Value::Int8(v) => v,
        ref v => panic!("integer:{v:?}"),
    }
}
fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, ro)
        .map(|x| {
            let (k, v) = x.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect()
}
#[derive(Default)]
struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, k: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(k.into_vec());
    }
    fn delete(&mut self, k: Box<[u8]>) {
        self.0.push(k.into_vec());
    }
}
#[derive(Clone, Copy, Debug)]
enum Change {
    Restart,
    Increment,
    Drop,
}
impl Change {
    fn sql(self) -> &'static str {
        match self {
            Self::Restart => "ALTER SEQUENCE target RESTART WITH 500",
            Self::Increment => "ALTER SEQUENCE target INCREMENT BY 3",
            Self::Drop => "DROP SEQUENCE target",
        }
    }
    fn expected(self) -> i64 {
        match self {
            Self::Restart => 500,
            Self::Increment => 152,
            Self::Drop => 1,
        }
    }
}
fn run(change: Change) {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            {
                let db = open(path.path(), history);
                exec(
                    &db,
                    params,
                    "CREATE SEQUENCE target START WITH 100 INCREMENT BY 7 CACHE 8",
                )
                .unwrap();
                assert_eq!(next(&db, params), 100);
            }
            // Fresh namespace: authoritative reservation149 exists, no runtime is loaded.
            let db = open(path.path(), history);
            let before = db.storage.db().latest_sequence_number();
            let (read_tx, read_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (attempt_tx, attempt_rx) = mpsc::channel();
            let (applied_tx, applied_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let (read_reached, attempt_reached, applied_early, completed_early, before_release, load, ddl) =
                std::thread::scope(|scope| {
                    let loader = scope.spawn(|| {
                        debug_next_after_cold_runtime_read(move || {
                            read_tx.send(()).unwrap();
                            release_rx
                                .recv_timeout(Duration::from_secs(20))
                                .expect("bounded loader release");
                        });
                        debug_load_runtime_on(&db.storage, "target")
                    });
                    let read_reached = read_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                    let writer = scope.spawn(|| {
                        debug_next_before_ddl_owner(move || {
                            attempt_tx.send(()).unwrap();
                        });
                        debug_next_after_native_commit(move || {
                            applied_tx.send(()).unwrap();
                        });
                        let result = exec(&db, params, change.sql());
                        let _ = done_tx.send(());
                        result
                    });
                    // Correct DDL waits BEFORE native while the cold factory owns
                    // this name. Establish its real owner attempt, then sample both
                    // native/public completion and the actual DB sequence.
                    let attempt_reached = attempt_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                    let applied_early = applied_rx.recv_timeout(Duration::from_millis(500)).is_ok();
                    let completed_early = done_rx.try_recv().is_ok();
                    let before_release = db.storage.db().latest_sequence_number();
                    // Always release/reap both workers before ANY assertion.
                    let _ = release_tx.send(());
                    (
                        read_reached,
                        attempt_reached,
                        applied_early,
                        completed_early,
                        before_release,
                        loader.join(),
                        writer.join(),
                    )
                });
            assert!(read_reached, "actual old definition/state load reached");
            assert!(attempt_reached, "actual managed DDL before-owner boundary reached");
            load.unwrap().unwrap();
            ddl.unwrap().unwrap();
            let applied_reached = applied_early || applied_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            assert!(
                applied_reached,
                "DDL eventually reaches actual NativeOK after factory release"
            );
            let raw = db.storage.db();
            let after = raw.latest_sequence_number();
            assert!(after > before);
            let mut owners = 0;
            let mut snapshots = 0;
            let mut expected_sequence = before + 1;
            let mut cursor = raw.get_updates_since(before).unwrap();
            for item in cursor.by_ref() {
                let (first, batch) = item.unwrap();
                assert_eq!(first, expected_sequence, "complete native sequence walk");
                expected_sequence += batch.len() as u64;
                let mut keys = Keys::default();
                batch.iterate(&mut keys);
                // The successful text statement's empty implicit transaction
                // may persist its established 1+snap bookkeeping after DDL.
                // Admit ONLY the exact four-key current snapshot group.
                let snapshot_key = |key: &[u8]| key.starts_with(b"snapshot:");
                let wall_key = |key: &[u8]| {
                    key.strip_prefix(b"scn_map:@")
                        .and_then(|rest| rest.first())
                        .is_some_and(u8::is_ascii_digit)
                };
                let snapshot_only = keys.0.len() == 4
                    && keys.0.iter().filter(|key| snapshot_key(key)).count() == 1
                    && keys.0.iter().filter(|key| wall_key(key)).count() == 1
                    && keys.0.iter().filter(|key| key.starts_with(b"scn_map:@s:")).count() == 1
                    && keys.0.iter().filter(|key| key.starts_with(b"scn_map:@t:")).count() == 1;
                if snapshot_only {
                    assert_eq!(owners, 1, "snapshot is post-owner bookkeeping");
                    snapshots += 1;
                    continue;
                }
                assert!(keys.0.iter().any(|k| k.starts_with(b"wal:entries:")));
                assert!(keys.0.iter().any(|k| k == b"meta:sequence:target"));
                owners += 1;
            }
            cursor.status().unwrap();
            assert_eq!(expected_sequence, after + 1);
            assert_eq!(owners, 1, "one metadata+WAL owner; no sequence repair/reservation");
            assert!(snapshots <= 1, "only the established 1+snap allowance");
            let bytes = image(&db);
            if matches!(change, Change::Drop) {
                assert!(db.storage.catalog().get_sequence("target").unwrap().is_none());
            } else {
                let def = db.storage.catalog().get_sequence("target").unwrap().unwrap();
                assert_eq!(
                    def.increment_by,
                    if matches!(change, Change::Increment) { 3 } else { 7 }
                );
            }
            assert_eq!(image(&db), bytes);
            assert_eq!(raw.latest_sequence_number(), after);
            // Judge the public wrong-result oracle first, even if the baseline DDL
            // completed early. A generation/wait diagnostic alone is not acceptance.
            assert_eq!(next(&db,params),change.expected(),"{change:?}/history={history}/params={params}: old decoder cannot republish its runtime after DDL completed");
            assert!(
                !applied_early,
                "managed DDL NativeOK must wait for factory owner release"
            );
            assert_eq!(
                before_release, before,
                "no native group while factory's definition/state cut is held"
            );
            assert!(
                !completed_early,
                "DDL publication must wait for the existing-definition load+insert owner"
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("sequence-cold-loader {change:?}=4");
}
#[test]
fn restart_discards_a_cold_runtime_read_before_its_publication() {
    run(Change::Restart);
}
#[test]
fn changed_increment_discards_a_cold_runtime_read_before_its_publication() {
    run(Change::Increment);
}
#[test]
fn drop_discards_a_cold_runtime_read_before_its_publication() {
    run(Change::Drop);
}
