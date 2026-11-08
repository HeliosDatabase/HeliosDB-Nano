//! Spec R5.3 (`docs/plans/specs/R5_3_ddl_batch.md`): every catalog DDL statement is ONE
//! durable write group.
//!
//! # How a statement's groups are counted
//!
//! `replication.physical.enabled = true` makes RocksDB archive its WAL files instead of
//! deleting them, so [`write_groups_since`] reads every write group the store made after
//! a sequence number (`DB::get_updates_since`), with its keys. A group a statement makes
//! that changes nothing a reader can see is set apart, with the reason
//! ([`invisible`]):
//!
//! * `snapshot:{ts}` — the record a text-family statement's implicit transaction
//!   registers at its (empty) commit after a pure DDL statement. Read only by `AS OF
//!   TRANSACTION/SCN` resolution and snapshot GC, for a transaction no client has been
//!   told of (design §7: "1 + snap"). Its `scn_map:@` lookup index is written
//!   in that same bookkeeping group. R5.4 folds it into non-empty commits.
//! * `wal:checkpoint` — the logical WAL's checkpoint (off in [`config`] anyway).
//! * `idxsnap*:` / `vecsnap*:` — derived index-snapshot artefacts.
//! * `v:` / `v_idx:` / `vmeta:` / `meta:history_purge:` alone — a dropped or renamed-away
//!   name's version history, purged in bounded chunks AFTER the statement's group, under
//!   a name that is no table (the group itself records the purge).
//!
//! Everything else is visible, and each converted statement must make exactly one such
//! group, on both executor families where the parser takes the statement.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::WriteBatchIterator;
use std::path::Path;
use std::time::Duration;

fn config(dir: &Path) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    // Archive obsolete RocksDB WAL files, so every group stays readable.
    c.replication.physical.enabled = true;
    c
}

/// Retried: a just-dropped handle's background threads release the RocksDB LOCK
/// asynchronously.
fn open(dir: &Path) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config(dir)) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("opening {} failed: {last:?}", dir.display());
}

/// One RocksDB write group: its keys.
#[derive(Debug)]
struct Group {
    keys: Vec<String>,
}

impl Group {
    fn has(&self, prefix: &str) -> bool {
        self.keys.iter().any(|key| key.starts_with(prefix))
    }
}

#[derive(Default)]
struct Keys(Vec<String>);

impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.0.push(String::from_utf8_lossy(&key).into_owned());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(String::from_utf8_lossy(&key).into_owned());
    }
}

fn latest(db: &EmbeddedDatabase) -> u64 {
    db.storage.db().latest_sequence_number()
}

/// Every write group the store made after sequence `after`, oldest first, up to the
/// store's latest sequence, with no gap: the walk starts at `after + 1` (the batch
/// holding `after` itself, written before the statement, is not counted:
/// `get_updates_since` skips every batch whose first sequence is at or below its
/// argument), and each batch begins where the one before it ended. A WAL file the
/// store had already let go of would show as a gap, not as fewer groups.
fn write_groups_since(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    let mut groups = Vec::new();
    let mut next = after + 1;
    let mut updates = raw
        .get_updates_since(after)
        .expect("the WAL from the statement on is retained");
    for item in updates.by_ref() {
        let (first, batch) = item.expect("read a WAL batch");
        if first > latest {
            break;
        }
        assert_eq!(
            first, next,
            "a gap in the WAL after sequence {after}: expected {next}, found {first}"
        );
        next = first + batch.len() as u64;
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        groups.push(Group { keys: keys.0 });
    }
    updates.status().expect("WAL iterator status");
    assert_eq!(
        next,
        latest + 1,
        "the walk after sequence {after} ended at {} but the store is at {latest}",
        next - 1
    );
    groups
}

/// A key no reader's answer depends on (see the module documentation).
fn invisible(key: &str) -> bool {
    key.starts_with("snapshot:")
        || key.starts_with("scn_map:@")
        || key.starts_with("wal:checkpoint")
        || key.starts_with("idxsnap")
        || key.starts_with("vecsnap")
}

/// A version-history purge chunk of a name that is no table.
fn history_purge(group: &Group) -> bool {
    group.keys.iter().all(|key| {
        key.starts_with("v:")
            || key.starts_with("v_idx:")
            || key.starts_with("vmeta:")
            || key.starts_with("meta:history_purge:")
    })
}

fn visible_groups(groups: Vec<Group>) -> Vec<Group> {
    groups
        .into_iter()
        .filter(|group| group.keys.iter().any(|key| !invisible(key)) && !history_purge(group))
        .collect()
}

/// Run `statement` and return the ONE visible write group it made.
fn one_group(db: &EmbeddedDatabase, what: &str, statement: impl FnOnce()) -> Group {
    let before = latest(db);
    statement();
    let mut visible = visible_groups(write_groups_since(db, before));
    assert_eq!(
        visible.len(),
        1,
        "{what}: expected ONE write group, got {}: {visible:#?}",
        visible.len()
    );
    visible.remove(0)
}

/// The two executor families: the text family (`execute`, the simple-query protocol,
/// MySQL) and the params family (`execute_params`, the extended protocol, REST).
#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
    .unwrap_or_else(|e| panic!("{family:?}: {sql}: {e}"));
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match rows.first().and_then(|row| row.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("{sql}: a count of {other:?}"),
    }
}

fn text(db: &EmbeddedDatabase, sql: &str) -> String {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match rows.first().and_then(|row| row.values.first()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{sql}: a text of {other:?}"),
    }
}

/// Every stored key under `prefix`.
fn stored_keys(db: &EmbeddedDatabase, prefix: &str) -> Vec<String> {
    let raw = db.storage.db();
    let mut options = rocksdb::ReadOptions::default();
    options.set_total_order_seek(true);
    let mut keys = Vec::new();
    for item in raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        options,
    ) {
        let (key, _) = item.expect("read a key");
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        keys.push(String::from_utf8_lossy(&key).into_owned());
    }
    keys
}

#[test]
fn create_table_is_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO parent VALUES (1)").unwrap();
    for family in FAMILIES {
        let table = format!("child_{family:?}").to_lowercase();
        let sql = format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, email TEXT UNIQUE, p INT REFERENCES parent(id), n INT, \
             CHECK (n > 0))"
        );
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        for key in [
            format!("meta:table:{table}"),
            format!("counter:{table}"),
            format!("table_constraints:{table}"),
            "wal:entries:".to_string(),
        ] {
            assert!(group.has(&key), "{family:?}: the group lacks {key}: {group:#?}");
        }
        // The constraints the one group carried are enforced.
        db.execute(&format!("INSERT INTO {table} VALUES (1, 'a', 1, 1)"))
            .unwrap();
        assert!(db
            .execute(&format!("INSERT INTO {table} VALUES (2, 'a', 1, 1)"))
            .is_err());
        assert!(db
            .execute(&format!("INSERT INTO {table} VALUES (3, 'b', 1, 0)"))
            .is_err());
    }
}

#[test]
fn a_partition_child_and_a_cascading_drop_are_one_write_group_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")
        .unwrap();
    let create = one_group(&db, "CREATE TABLE … PARTITION OF", || {
        db.execute("CREATE TABLE child_low PARTITION OF parent FOR VALUES FROM (0) TO (100)")
            .unwrap();
    });
    assert!(create.has("meta:table:child_low"), "{create:#?}");
    assert!(create.has("meta:partchild:parent"), "{create:#?}");
    assert!(create.has("meta:partparent:child_low"), "{create:#?}");
    db.execute("CREATE TABLE child_high PARTITION OF parent FOR VALUES FROM (100) TO (200)")
        .unwrap();

    let cascade = one_group(&db, "DROP TABLE parent", || {
        db.execute("DROP TABLE parent").unwrap();
    });
    for key in ["meta:table:parent", "meta:table:child_low", "meta:table:child_high"] {
        assert!(cascade.has(key), "the cascade's group lacks {key}: {cascade:#?}");
    }
    assert!(db.query("SELECT * FROM child_low", &[]).is_err());
    assert!(db.query("SELECT * FROM child_high", &[]).is_err());
    // The parent's child list went with it, and is not written back by a child.
    let raw = db.storage.db();
    assert!(raw.get(b"meta:partchild:parent").unwrap().is_none());
}

#[test]
fn rename_table_carries_its_indexes_and_triggers_in_its_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE INDEX r_v_idx ON r (v)").unwrap();
    db.execute("INSERT INTO r VALUES (1, 'a'), (2, 'b')").unwrap();
    db.execute("CREATE FUNCTION rf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$")
        .unwrap();
    db.execute("CREATE TRIGGER rt BEFORE INSERT ON r FOR EACH ROW EXECUTE FUNCTION rf()")
        .unwrap();
    let mut name = "r".to_string();
    for (n, family) in FAMILIES.into_iter().enumerate() {
        let next = format!("r{n}");
        let sql = format!("ALTER TABLE {name} RENAME TO {next}");
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        for key in [
            format!("meta:table:{next}"),
            format!("data:{next}:"),
            "meta:index:r_v_idx".to_string(),
            format!("trigger:{next}:rt"),
            "wal:entries:".to_string(),
        ] {
            assert!(
                group.has(&key),
                "{family:?}: the rename's group lacks {key}: {group:#?}"
            );
        }
        assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {next} WHERE v = 'b'")), 1);
        assert!(db.storage.art_indexes().find_column_index(&next, "v").is_some());
        name = next;
    }
}

#[test]
fn rename_column_is_one_write_group_and_keeps_unique_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE rc (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO rc VALUES (1, 'x')").unwrap();
    let mut column = "code".to_string();
    for (n, family) in FAMILIES.into_iter().enumerate() {
        let next = format!("label{n}");
        let sql = format!("ALTER TABLE rc RENAME COLUMN {column} TO {next}");
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        assert!(group.has("meta:table:rc"), "{group:#?}");
        assert!(group.has("table_constraints:rc"), "{group:#?}");
        assert!(
            db.execute(&format!("INSERT INTO rc (id, {next}) VALUES ({}, 'x')", n + 2))
                .is_err(),
            "{family:?}: the renamed column is still UNIQUE"
        );
        column = next;
    }
}

/// Review F8: the side storage keyed by the table's name — a DICTIONARY column's
/// dictionary, a COLUMNAR column's batches, zone stats and live-row presence — moves
/// with `RENAME TO`, in its write group. Red before: every read of the renamed
/// DICTIONARY column failed (`Invalid dict_id 0`), and so did its `TRUNCATE`; the
/// renamed COLUMNAR column read as empty (`WHERE v < 50` gave 0 rows, not 49).
#[test]
fn rename_table_carries_its_dictionary_and_columnar_side_storage() {
    fn reads_back(db: &EmbeddedDatabase, table: &str, when: &str) {
        assert_eq!(
            count(db, &format!("SELECT COUNT(*) FROM {table} WHERE v < 50")),
            49,
            "{when}: the COLUMNAR column"
        );
        assert_eq!(
            count(db, &format!("SELECT v FROM {table} WHERE id = 101")),
            101,
            "{when}"
        );
        assert_eq!(text(db, &format!("SELECT s FROM {table} WHERE id = 7")), "s2", "{when}");
        assert_eq!(
            text(db, &format!("SELECT s FROM {table} WHERE id = 101")),
            "new",
            "{when}"
        );
        assert_eq!(
            count(db, &format!("SELECT COUNT(*) FROM {table} WHERE s = 's3'")),
            20,
            "{when}: the DICTIONARY column"
        );
    }
    for family in FAMILIES {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path());
            db.execute("CREATE TABLE sd (id INT PRIMARY KEY, v INT, s TEXT)")
                .unwrap();
            db.execute("INSERT INTO sd SELECT x, x, 's' || CAST(x % 5 AS TEXT) FROM generate_series(1, 100) x")
                .unwrap();
            db.execute("ALTER TABLE sd ALTER COLUMN s SET STORAGE DICTIONARY")
                .unwrap();
            db.execute("ALTER TABLE sd ALTER COLUMN v SET STORAGE COLUMNAR")
                .unwrap();
            db.execute("INSERT INTO sd VALUES (101, 101, 'new')").unwrap();
            reads_back(&db, "sd", "before the rename");
            for prefix in ["dict:sd:s", "col:sd:v:"] {
                assert!(
                    !stored_keys(&db, prefix).is_empty(),
                    "vacuity: {prefix} is stored before the rename"
                );
            }
            // Each family stored under the old name, by its prefix under the new one.
            let moving: Vec<String> = ["dict:sd:", "col:sd:", "colz:sd:", "colzm:sd:", "colp:sd:"]
                .into_iter()
                .filter(|prefix| !stored_keys(&db, prefix).is_empty())
                .map(|prefix| prefix.replace(":sd:", ":sd_r:"))
                .collect();

            let before = latest(&db);
            run(&db, family, "ALTER TABLE sd RENAME TO sd_r");
            let groups = write_groups_since(&db, before);
            let rename = groups
                .iter()
                .find(|group| group.has("meta:table:sd_r"))
                .unwrap_or_else(|| panic!("{family:?}: no group wrote meta:table:sd_r: {groups:#?}"));
            for key in &moving {
                assert!(
                    rename.has(key),
                    "{family:?}: the rename's group lacks {key}: {rename:#?}"
                );
            }
            reads_back(&db, "sd_r", &format!("{family:?}: after the rename"));
        }
        let db = open(dir.path());
        reads_back(&db, "sd_r", &format!("{family:?}: after a reopen"));
        for prefix in ["dict:sd:", "col:sd:", "colz:sd:", "colzm:sd:", "colp:sd:"] {
            assert_eq!(
                stored_keys(&db, prefix),
                Vec::<String>::new(),
                "{family:?}: left under the old name"
            );
        }
        db.execute("TRUNCATE sd_r").unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM sd_r"), 0);
    }
}

/// Review F8: `RENAME COLUMN` moves the column's dictionary and columnar side data
/// with it. Red before: the renamed DICTIONARY column read nothing back, a value
/// inserted under the new name went into a second dictionary, and after the rename
/// back that row read another value; the renamed COLUMNAR column read as empty.
#[test]
fn rename_column_carries_its_dictionary_and_columnar_side_storage() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        db.execute("CREATE TABLE dc (id INT PRIMARY KEY, s TEXT, v INT)")
            .unwrap();
        db.execute("INSERT INTO dc SELECT x, 's' || CAST(x % 5 AS TEXT), x FROM generate_series(1, 100) x")
            .unwrap();
        db.execute("ALTER TABLE dc ALTER COLUMN s SET STORAGE DICTIONARY")
            .unwrap();
        db.execute("ALTER TABLE dc ALTER COLUMN v SET STORAGE COLUMNAR")
            .unwrap();
        for (n, family) in FAMILIES.into_iter().enumerate() {
            run(&db, family, "ALTER TABLE dc RENAME COLUMN s TO s2");
            run(&db, family, "ALTER TABLE dc RENAME COLUMN v TO v2");
            assert_eq!(text(&db, "SELECT s2 FROM dc WHERE id = 1"), "s1", "{family:?}");
            assert_eq!(count(&db, "SELECT COUNT(*) FROM dc WHERE v2 < 50"), 49, "{family:?}");
            let id = 1000 + n;
            db.execute(&format!("INSERT INTO dc VALUES ({id}, 'brandnew{n}', {id})"))
                .unwrap();
            assert_eq!(
                text(&db, &format!("SELECT s2 FROM dc WHERE id = {id}")),
                format!("brandnew{n}"),
                "{family:?}"
            );

            run(&db, family, "ALTER TABLE dc RENAME COLUMN s2 TO s");
            run(&db, family, "ALTER TABLE dc RENAME COLUMN v2 TO v");
            assert_eq!(text(&db, "SELECT s FROM dc WHERE id = 1"), "s1", "{family:?}");
            assert_eq!(
                text(&db, &format!("SELECT s FROM dc WHERE id = {id}")),
                format!("brandnew{n}"),
                "{family:?}: a value inserted between the renames reads back after the rename back"
            );
            assert_eq!(count(&db, "SELECT COUNT(*) FROM dc WHERE v < 50"), 49, "{family:?}");
            assert_eq!(
                count(&db, &format!("SELECT v FROM dc WHERE id = {id}")),
                id as i64,
                "{family:?}"
            );
            for prefix in ["dict:dc:s2", "col:dc:v2:", "colz:dc:v2:", "colzm:dc:v2"] {
                assert_eq!(
                    stored_keys(&db, prefix),
                    Vec::<String>::new(),
                    "{family:?}: left under s2/v2"
                );
            }
        }
    }
    let db = open(dir.path());
    assert_eq!(text(&db, "SELECT s FROM dc WHERE id = 1"), "s1");
    assert_eq!(text(&db, "SELECT s FROM dc WHERE id = 1000"), "brandnew0");
    assert_eq!(text(&db, "SELECT s FROM dc WHERE id = 1001"), "brandnew1");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM dc WHERE s = 's3'"), 20);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM dc WHERE v < 50"), 49);
}

/// Review F8: a renamed table's trigger fires and drops under its new name, and a
/// drop sticks. Red before: the live registry kept the trigger under the old name,
/// so it stopped firing, `DROP TRIGGER … ON <new>` failed, and `DROP TRIGGER … ON
/// <old>` succeeded without touching the record, which brought the trigger back at
/// the next open.
#[test]
fn a_renamed_tables_trigger_fires_and_drops_under_the_new_name() {
    for family in FAMILIES {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path());
            db.execute("CREATE TABLE tg (id INT PRIMARY KEY, v INT)").unwrap();
            db.execute(
                "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.v = NEW.v * 10; \
                 RETURN NEW; END $$",
            )
            .unwrap();
            db.execute("CREATE TRIGGER tt BEFORE INSERT ON tg FOR EACH ROW EXECUTE FUNCTION tf()")
                .unwrap();
            db.execute("INSERT INTO tg VALUES (1, 1)").unwrap();
            assert_eq!(
                count(&db, "SELECT v FROM tg WHERE id = 1"),
                10,
                "vacuity: the trigger fires"
            );

            run(&db, family, "ALTER TABLE tg RENAME TO tg2");
            db.execute("INSERT INTO tg2 VALUES (2, 2)").unwrap();
            assert_eq!(
                count(&db, "SELECT v FROM tg2 WHERE id = 2"),
                20,
                "{family:?}: the trigger fires under the new name"
            );
            assert!(
                db.execute("DROP TRIGGER tt ON tg").is_err(),
                "{family:?}: no trigger is left under the old name"
            );
            run(&db, family, "DROP TRIGGER tt ON tg2");
            db.execute("INSERT INTO tg2 VALUES (3, 3)").unwrap();
            assert_eq!(count(&db, "SELECT v FROM tg2 WHERE id = 3"), 3, "{family:?}: dropped");
        }
        let db = open(dir.path());
        db.execute("INSERT INTO tg2 VALUES (4, 4)").unwrap();
        assert_eq!(
            count(&db, "SELECT v FROM tg2 WHERE id = 4"),
            4,
            "{family:?}: the dropped trigger stays dropped after a reopen"
        );
        assert_eq!(stored_keys(&db, "trigger:tg"), Vec::<String>::new(), "{family:?}");
    }
}

/// Review F11: `pg_indexes` never shows part of a table's index set while a `RENAME
/// TO` or a `DROP TABLE` publishes. The user indexes came from the in-memory registry,
/// which those statements change after their durable group, one index at a time.
#[test]
fn pg_indexes_never_shows_part_of_a_tables_index_set() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    fn index_names(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
        let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        rows.iter()
            .map(|row| match row.values.first() {
                Some(Value::String(name)) => name.clone(),
                other => panic!("{sql}: an index name of {other:?}"),
            })
            .collect()
    }
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE pi (id INT PRIMARY KEY, a INT, b INT, c INT)")
        .unwrap();
    for column in ["a", "b", "c"] {
        db.execute(&format!("CREATE INDEX pi_{column} ON pi ({column})"))
            .unwrap();
    }
    let done = AtomicBool::new(false);
    // Odd: `pd` was complete when it was set, and the DROP TABLE of it may be running.
    let phase = AtomicU64::new(0);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut n = 0u64;
            while !done.load(Ordering::Acquire) {
                n += 1;
                // Distinct texts, so no answer comes from the result cache.
                let names = index_names(
                    &db,
                    &format!("SELECT indexname FROM pg_indexes WHERE tablename IN ('pi', 'pj') AND {n} > 0"),
                );
                assert_eq!(names.len(), 4, "part of a renamed table's index set: {names:?}");
                let before = phase.load(Ordering::Acquire);
                let names = index_names(
                    &db,
                    &format!("SELECT indexname FROM pg_indexes WHERE tablename = 'pd' AND {n} > 0"),
                );
                if before % 2 == 1 && phase.load(Ordering::Acquire) == before {
                    assert!(
                        names.is_empty() || names.len() == 4,
                        "part of a dropped table's index set: {names:?}"
                    );
                }
            }
            n
        });
        for n in 0..20u64 {
            db.execute("ALTER TABLE pi RENAME TO pj").unwrap();
            db.execute("ALTER TABLE pj RENAME TO pi").unwrap();
            db.execute("CREATE TABLE pd (id INT PRIMARY KEY, a INT, b INT, c INT)")
                .unwrap();
            for column in ["a", "b", "c"] {
                db.execute(&format!("CREATE INDEX pd_{column}_{n} ON pd ({column})"))
                    .unwrap();
            }
            phase.store(2 * n + 1, Ordering::Release);
            db.execute("DROP TABLE pd").unwrap();
            phase.store(2 * n + 2, Ordering::Release);
        }
        done.store(true, Ordering::Release);
        assert!(reader.join().unwrap() > 0, "the reader read");
    });
}

#[test]
fn create_and_drop_index_are_one_write_group_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE ix (id INT PRIMARY KEY, v TEXT, w TEXT)")
        .unwrap();
    db.execute("INSERT INTO ix VALUES (1, 'a', 'p'), (2, 'b', 'q')")
        .unwrap();
    for family in FAMILIES {
        let index = format!("ix_v_{family:?}").to_lowercase();
        let unique = format!("ix_w_{family:?}").to_lowercase();
        let create = format!("CREATE INDEX {index} ON ix (v)");
        let group = one_group(&db, &create, || run(&db, family, &create));
        assert!(group.has(&format!("meta:index:{index}")), "{group:#?}");
        assert!(group.has("wal:entries:"), "{group:#?}");
        assert!(db.storage.art_indexes().find_column_index("ix", "v").is_some());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM ix WHERE v = 'b'"), 1);

        let create_unique = format!("CREATE UNIQUE INDEX {unique} ON ix (w)");
        let group = one_group(&db, &create_unique, || run(&db, family, &create_unique));
        assert!(group.has(&format!("meta:index:{unique}")), "{group:#?}");
        assert!(db.execute("INSERT INTO ix VALUES (9, 'z', 'p')").is_err());

        for name in [&index, &unique] {
            let drop = format!("DROP INDEX {name}");
            let group = one_group(&db, &drop, || run(&db, family, &drop));
            assert!(group.has(&format!("meta:index:{name}")), "{group:#?}");
            assert!(!db.storage.art_indexes().index_exists(name));
        }
    }
}

#[test]
fn constraint_ddl_is_one_write_group_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE cp (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE cc (id INT PRIMARY KEY, pid INT, u TEXT)")
        .unwrap();
    db.execute("INSERT INTO cp VALUES (1)").unwrap();
    db.execute("INSERT INTO cc VALUES (1, 1, 'a')").unwrap();
    for (n, family) in FAMILIES.into_iter().enumerate() {
        let unique = format!("cc_u_{n}");
        let sql = format!("ALTER TABLE cc ADD CONSTRAINT {unique} UNIQUE (u)");
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        assert!(group.has("table_constraints:cc"), "{group:#?}");
        assert!(group.has("wal:entries:"), "{group:#?}");
        assert!(db
            .execute(&format!("INSERT INTO cc VALUES ({}, 1, 'a')", n + 10))
            .is_err());
        let sql = format!("ALTER TABLE cc DROP CONSTRAINT {unique}");
        one_group(&db, &sql, || run(&db, family, &sql));

        let fk = format!("cc_fk_{n}");
        let sql = format!("ALTER TABLE cc ADD CONSTRAINT {fk} FOREIGN KEY (pid) REFERENCES cp(id)");
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        assert!(group.has("table_constraints:cc"), "{group:#?}");
        let sql = format!("ALTER TABLE cc DROP CONSTRAINT {fk}");
        one_group(&db, &sql, || run(&db, family, &sql));
    }
}

#[test]
fn routine_view_trigger_and_sequence_ddl_are_one_write_group_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE base (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$")
        .unwrap();
    let statements = [
        ("CREATE VIEW bv AS SELECT id FROM base", "__view_metadata__bv"),
        ("DROP VIEW bv", "__view_metadata__bv"),
        (
            "CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
            "meta:function:",
        ),
        ("DROP FUNCTION dbl", "meta:function:"),
        (
            "CREATE PROCEDURE bp(a INT) LANGUAGE sql AS $$INSERT INTO base VALUES ($a, $a)$$",
            "meta:procedure:",
        ),
        ("DROP PROCEDURE bp", "meta:procedure:"),
        (
            "CREATE TRIGGER bt BEFORE INSERT ON base FOR EACH ROW EXECUTE FUNCTION tf()",
            "trigger:base:bt",
        ),
        ("DROP TRIGGER bt ON base", "trigger:base:bt"),
        ("CREATE SEQUENCE bs START 5", "meta:sequence:bs"),
        ("ALTER SEQUENCE bs RESTART WITH 10", "meta:sequence:bs"),
        ("DROP SEQUENCE bs", "meta:sequence:bs"),
    ];
    for family in FAMILIES {
        for (sql, key) in statements {
            let group = one_group(&db, sql, || run(&db, family, sql));
            assert!(group.has(key), "{family:?}: {sql}: the group lacks {key}: {group:#?}");
            assert!(group.has("wal:entries:"), "{family:?}: {sql}: {group:#?}");
        }
    }
}

/// A refused `RESTART` used to leave the ALTER's new definition behind: it was
/// validated after the definition was written.
#[test]
fn alter_sequence_restart_out_of_bounds_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE SEQUENCE sb MINVALUE 1 MAXVALUE 10").unwrap();
    let before = latest(&db);
    let refused = db
        .execute("ALTER SEQUENCE sb MAXVALUE 20 RESTART WITH 50")
        .expect_err("RESTART past MAXVALUE");
    // Review F17: classified as an invalid parameter (22023 on the wire), not XX000.
    assert!(refused.to_string().contains("invalid sequence parameter"), "{refused}");
    assert!(
        visible_groups(write_groups_since(&db, before)).is_empty(),
        "a refused ALTER SEQUENCE wrote nothing"
    );
    // The old bound holds: 11 is past MAXVALUE 10.
    assert!(db.execute("ALTER SEQUENCE sb RESTART WITH 11").is_err());
}

#[test]
fn refresh_materialized_view_is_one_write_group_with_one_snapshot_record() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, g INT)").unwrap();
    for chunk in 0..3 {
        let values: Vec<String> = (0..500)
            .map(|i| {
                let id = chunk * 500 + i;
                format!("({id}, {})", id % 7)
            })
            .collect();
        db.execute(&format!("INSERT INTO src VALUES {}", values.join(", ")))
            .unwrap();
    }
    // A projection, so the data table carries no key flag (and no index) of `src`.
    db.execute("CREATE MATERIALIZED VIEW mv_big AS SELECT id * 1 AS id, g FROM src")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv_g AS SELECT g, COUNT(*) AS n FROM src GROUP BY g")
        .unwrap();
    for sql in [
        "REFRESH MATERIALIZED VIEW mv_big",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv_big",
        "REFRESH MATERIALIZED VIEW mv_g",
    ] {
        let before = latest(&db);
        db.execute(sql).unwrap();
        let groups = write_groups_since(&db, before);
        // A REFRESH of 1000 rows or more wrote one snapshot record per row, and only
        // the last 1000 survived a restart.
        let snapshots = groups.iter().filter(|group| group.has("snapshot:")).count();
        assert!(snapshots <= 2, "{sql}: {snapshots} snapshot records");
        let visible = visible_groups(groups);
        assert_eq!(visible.len(), 1, "{sql}: {visible:#?}");
        let view = if sql.ends_with("mv_g") { "mv_g" } else { "mv_big" };
        assert!(visible[0].has(&format!("meta:mv:{view}")), "{sql}: {:#?}", visible[0]);
        assert!(
            visible[0].has(&format!("data:__mv_{view}:")),
            "{sql}: {:#?}",
            visible[0]
        );
    }
    assert_eq!(count(&db, "SELECT COUNT(*) FROM mv_big"), 1500);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM mv_g"), 7);
    drop(db);
    let db = open(dir.path());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM mv_big"), 1500);
}

/// A killed or a watched REFRESH used to show the view empty, then partial (48 to 1018
/// of 2000 rows): it dropped the data table and inserted row by row.
#[test]
fn refresh_materialized_view_never_shows_an_empty_or_partial_view() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, g INT)").unwrap();
    let values: Vec<String> = (0..400).map(|id| format!("({id}, {})", id % 5)).collect();
    db.execute(&format!("INSERT INTO src VALUES {}", values.join(", ")))
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id * 1 AS id, g FROM src")
        .unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut seen = 0usize;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                assert_eq!(
                    count(&db, "SELECT COUNT(*) FROM mv"),
                    400,
                    "a reader saw a partial view"
                );
                seen += 1;
            }
            seen
        });
        for n in 0..30 {
            let sql = if n % 2 == 0 {
                "REFRESH MATERIALIZED VIEW mv"
            } else {
                "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"
            };
            db.execute(sql).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        assert!(reader.join().unwrap() > 0, "the reader read");
    });
}

/// `REFRESH` and `DROP MATERIALIZED VIEW` did not parse on the params family (the
/// extended protocol, REST).
#[test]
fn refresh_and_drop_materialized_view_run_on_the_params_family() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, g INT)").unwrap();
    db.execute("INSERT INTO src VALUES (1, 1), (2, 2)").unwrap();
    db.execute("CREATE MATERIALIZED VIEW pmv AS SELECT id * 1 AS id, g FROM src")
        .unwrap();
    db.execute("INSERT INTO src VALUES (3, 3)").unwrap();
    let group = one_group(&db, "REFRESH (params)", || {
        db.execute_params("REFRESH MATERIALIZED VIEW pmv", &[]).unwrap();
    });
    assert!(group.has("meta:mv:pmv"), "{group:#?}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM pmv"), 3);
    let group = one_group(&db, "DROP MATERIALIZED VIEW (params)", || {
        db.execute_params("DROP MATERIALIZED VIEW pmv", &[]).unwrap();
    });
    assert!(group.has("meta:mv:pmv"), "{group:#?}");
    assert!(group.has("meta:table:__mv_pmv"), "{group:#?}");
    assert!(db.query("SELECT * FROM pmv", &[]).is_err());
}

/// `EXPLAIN ANALYZE TRUNCATE` deleted at once, outside the statement's transaction:
/// a `ROLLBACK` restored nothing. It is refused before anything runs (PostgreSQL has no
/// EXPLAIN of TRUNCATE either).
#[test]
fn explain_analyze_truncate_in_a_transaction_is_refused_and_keeps_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE et (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO et VALUES (1), (2), (3)").unwrap();
    db.execute("BEGIN").unwrap();
    let refused = db.query("EXPLAIN ANALYZE TRUNCATE et", &[]).unwrap_err().to_string();
    assert!(
        refused.contains(heliosdb_nano::EXPLAIN_ANALYZE_UNSUPPORTED_MARKER),
        "{refused}"
    );
    let _ = db.execute("ROLLBACK");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM et"), 3);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM et WHERE id = 2"), 1);
}

/// TRUNCATE's ART clear runs once its commit has written the deletes.
#[test]
fn truncate_leaves_no_index_entries_and_takes_its_keys_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE tr (id INT PRIMARY KEY, u TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO tr VALUES (1, 'a'), (2, 'b')").unwrap();
    db.execute("TRUNCATE tr").unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM tr WHERE id = 1"), 0);
    db.execute("INSERT INTO tr VALUES (1, 'a')").unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM tr"), 1);
}

/// Decision 6: a group above the ceiling is refused before RocksDB sees it.
#[test]
fn a_write_group_above_the_ceiling_is_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let gate = std::sync::Arc::clone(db.storage.write_gate());
    let before = latest(&db);
    gate.debug_set_write_group_ceiling(Some(64));
    let refused = db.execute("CREATE TABLE ceil (id INT PRIMARY KEY, v TEXT)");
    gate.debug_set_write_group_ceiling(None);
    let refused = refused.unwrap_err().to_string();
    assert!(refused.contains("too large for one atomic write"), "{refused}");
    assert!(
        !write_groups_since(&db, before)
            .iter()
            .any(|group| group.has("meta:table:ceil")),
        "nothing of the statement was written"
    );
    assert!(db.query("SELECT * FROM ceil", &[]).is_err());
    db.execute("CREATE TABLE ceil (id INT PRIMARY KEY, v TEXT)").unwrap();
}

/// No converted statement wrote outside its batch (a test build panics at the write;
/// this also holds a release build to it).
#[test]
fn no_statement_wrote_outside_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let before = heliosdb_nano::storage::ddl_batch::side_writes();
    db.execute("CREATE TABLE sw (id INT PRIMARY KEY, v TEXT UNIQUE)")
        .unwrap();
    db.execute("CREATE INDEX sw_v ON sw (v)").unwrap();
    db.execute("ALTER TABLE sw RENAME TO sw2").unwrap();
    db.execute("ALTER TABLE sw2 RENAME COLUMN v TO w").unwrap();
    db.execute("DROP INDEX sw_v").unwrap();
    db.execute("DROP TABLE sw2").unwrap();
    assert_eq!(heliosdb_nano::storage::ddl_batch::side_writes(), before);
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    use heliosdb_nano::storage::ddl_batch::debug_fail_next_commit;

    /// A statement whose group is not written publishes nothing: no cache, no
    /// registry entry, no ART tree; and the retry succeeds.
    #[test]
    fn ddl_commit_failure_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());

        debug_fail_next_commit();
        assert!(db
            .execute("CREATE TABLE f (id INT PRIMARY KEY, u TEXT UNIQUE)")
            .is_err());
        assert!(db.query("SELECT * FROM f", &[]).is_err(), "no table");
        assert!(db.storage.art_indexes().list_table_indexes("f").is_empty(), "no tree");
        db.execute("CREATE TABLE f (id INT PRIMARY KEY, u TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO f VALUES (1, 'a')").unwrap();

        debug_fail_next_commit();
        assert!(db.execute("CREATE INDEX f_u ON f (u)").is_err());
        assert!(!db.storage.art_indexes().index_exists("f_u"), "no index");
        db.execute("CREATE INDEX f_u ON f (u)").unwrap();
        assert!(db.storage.art_indexes().find_column_index("f", "u").is_some());

        debug_fail_next_commit();
        assert!(db
            .execute("CREATE FUNCTION fx(x INT) RETURNS INT AS $$ SELECT $1 $$ LANGUAGE sql")
            .is_err());
        assert!(!db.function_registry.function_exists("fx"), "no function");

        debug_fail_next_commit();
        assert!(db.execute("ALTER TABLE f RENAME TO g").is_err());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM f"), 1);
        db.execute("ALTER TABLE f RENAME TO g").unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM g"), 1);
    }

    /// Set by the parent on its re-exec of this test binary: the data directory the
    /// crash child works on.
    const CRASH_CHILD_ENV: &str = "HELIOS_R53_CRASH_CHILD_DB_PATH";
    /// The engine's failpoint: end the process right after one DDL's write group.
    const EXIT_AFTER_DDL_WRITE_ENV: &str = "HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE";

    /// Re-exec this test binary, filtered to `test_name`, as the crash child; it dies at
    /// the failpoint `exit_at` arms, or at the end of its workload, with no `Drop`.
    fn crash_child(test_name: &str, dir: &Path, exit_at: &str) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CRASH_CHILD_ENV, dir)
            .env(EXIT_AFTER_DDL_WRITE_ENV, exit_at)
            .status()
            .unwrap();
        assert!(status.success(), "the crash child of {test_name} failed: {status:?}");
    }

    /// The rename used to write its index-definition rewrites and trigger moves after
    /// its group: a kill 14-30 ms in left the renamed table without its secondary
    /// indexes and its trigger. Now a crash right after the group keeps both, and one
    /// before it keeps the table as it was.
    #[test]
    fn rename_keeps_its_indexes_and_triggers_at_every_kill() {
        const NAME: &str = "internal::rename_keeps_its_indexes_and_triggers_at_every_kill";
        if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
            let db = open(Path::new(&path));
            db.execute("ALTER TABLE r RENAME TO r2").unwrap();
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path());
            db.execute("CREATE TABLE r (id INT PRIMARY KEY, v TEXT)").unwrap();
            db.execute("CREATE INDEX r_v_idx ON r (v)").unwrap();
            db.execute("INSERT INTO r VALUES (1, 'a'), (2, 'b')").unwrap();
            db.execute("CREATE FUNCTION rf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$")
                .unwrap();
            db.execute("CREATE TRIGGER rt BEFORE INSERT ON r FOR EACH ROW EXECUTE FUNCTION rf()")
                .unwrap();
        }
        crash_child(NAME, dir.path(), "rename:r");
        let db = open(dir.path());
        assert!(db.query("SELECT * FROM r", &[]).is_err(), "the old name is gone");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM r2 WHERE v = 'b'"), 1);
        assert!(
            db.storage.art_indexes().find_column_index("r2", "v").is_some(),
            "the secondary index came back on the new name"
        );
        assert!(db.trigger_registry.has_triggers_for_table("r2"), "the trigger moved");
        assert!(!db.trigger_registry.has_triggers_for_table("r"));
        // The index is the user's: DROP INDEX finds its definition.
        db.execute("DROP INDEX r_v_idx").unwrap();
    }

    /// A crash right after CREATE TABLE's group: the table comes back with every
    /// constraint (it used to come back without them when the crash fell between its
    /// groups).
    #[test]
    fn create_table_crash_after_its_group_keeps_its_constraints() {
        const NAME: &str = "internal::create_table_crash_after_its_group_keeps_its_constraints";
        if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
            let db = open(Path::new(&path));
            db.execute("CREATE TABLE ct (id INT PRIMARY KEY, u TEXT UNIQUE, n INT, CHECK (n > 0))")
                .unwrap();
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()));
        crash_child(NAME, dir.path(), "create:ct");
        let db = open(dir.path());
        db.execute("INSERT INTO ct VALUES (1, 'a', 1)").unwrap();
        assert!(db.execute("INSERT INTO ct VALUES (2, 'a', 1)").is_err(), "UNIQUE");
        assert!(db.execute("INSERT INTO ct VALUES (3, 'b', 0)").is_err(), "CHECK");
    }

    /// Review F8: a rename whose group is not written leaves the dictionaries where
    /// they were, with the values not yet written among them (the move marks them
    /// written; the unwritten group marks them back), and the retry carries them.
    #[test]
    fn an_unwritten_rename_keeps_the_dictionaries_where_they_were() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path());
            db.execute("CREATE TABLE ud (id INT PRIMARY KEY, s TEXT)").unwrap();
            db.execute("INSERT INTO ud VALUES (1, 'a'), (2, 'b')").unwrap();
            db.execute("ALTER TABLE ud ALTER COLUMN s SET STORAGE DICTIONARY")
                .unwrap();
            db.execute("INSERT INTO ud VALUES (3, 'later')").unwrap();

            debug_fail_next_commit();
            assert!(db.execute("ALTER TABLE ud RENAME TO ud2").is_err());
            assert_eq!(text(&db, "SELECT s FROM ud WHERE id = 3"), "later");
            debug_fail_next_commit();
            assert!(db.execute("ALTER TABLE ud RENAME COLUMN s TO s2").is_err());
            assert_eq!(text(&db, "SELECT s FROM ud WHERE id = 3"), "later");

            db.execute("ALTER TABLE ud RENAME TO ud2").unwrap();
            assert_eq!(text(&db, "SELECT s FROM ud2 WHERE id = 3"), "later");
        }
        let db = open(dir.path());
        assert_eq!(text(&db, "SELECT s FROM ud2 WHERE id = 1"), "a");
        assert_eq!(text(&db, "SELECT s FROM ud2 WHERE id = 3"), "later");
    }
}
