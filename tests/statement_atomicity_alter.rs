//! Spec R5.5 (`docs/plans/specs/R5_5_drop_column_and_multi_alter.md`): `DROP COLUMN`, an
//! `ALTER TABLE` of several clauses and `SET STORAGE` are ONE write group each, a refusal
//! by any clause leaves nothing written, and a read at a snapshot older than a change of
//! the table's layout (`AS OF`, a transaction begun before it) reads each value under its
//! own column, or is refused (0A000), never shifted.
//!
//! Groups are counted as `tests/statement_atomicity_ddl.rs` counts them: the store archives
//! its RocksDB WAL (`replication.physical.enabled`), and [`write_groups_since`] reads every
//! group after a sequence number with its keys. A group of a dropped name's history purge
//! or of `snapshot:` records alone changes nothing a reader sees, and is set apart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{Config, EmbeddedDatabase, Value, BRANCH_ANCHOR_UNREADABLE_MARKER};
use rocksdb::WriteBatchIterator;
use std::path::Path;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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
fn open_with(config: Config) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config.clone()) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("open failed: {last:?}");
}

fn open(dir: &Path) -> EmbeddedDatabase {
    open_with(config(dir))
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

/// Every write group after sequence `after`, oldest first, with no gap.
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
        assert_eq!(first, next, "a gap in the WAL after sequence {after}");
        next = first + batch.len() as u64;
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        groups.push(Group { keys: keys.0 });
    }
    updates.status().expect("WAL iterator status");
    assert_eq!(next, latest + 1, "the walk after sequence {after} ended short");
    groups
}

/// A key no reader's answer depends on (`tests/statement_atomicity_ddl.rs`).
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

fn attempt(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) {
    attempt(db, family, sql).unwrap_or_else(|e| panic!("{family:?}: {sql}: {e}"));
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .into_iter()
        .map(|tuple| tuple.values)
        .collect()
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn ints(rows: &[Vec<Value>]) -> Vec<Vec<i64>> {
    rows.iter().map(|row| row.iter().map(int).collect()).collect()
}

fn s(text: &str) -> Value {
    Value::String(text.to_string())
}

fn column_names(db: &EmbeddedDatabase, table: &str) -> Vec<String> {
    db.storage
        .catalog()
        .get_table_schema(table)
        .unwrap()
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect()
}

/// The arity of every stored row of `table`, in key order.
fn stored_widths(db: &EmbeddedDatabase, table: &str) -> Vec<usize> {
    let prefix = format!("data:{table}:");
    let raw = db.storage.db();
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read,
    )
    .map(|item| item.unwrap())
    .take_while(|(key, _)| key.starts_with(prefix.as_bytes()))
    .map(|(_, value)| {
        let tuple: heliosdb_nano::Tuple = bincode::deserialize(&value).unwrap();
        tuple.values.len()
    })
    .collect()
}

fn sql_now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `AS OF TIMESTAMP` resolves to the second: a pause either side of a mark keeps
/// every write on its own side of it.
fn pause() {
    std::thread::sleep(Duration::from_millis(1200));
}

fn mark() -> String {
    pause();
    let now = sql_now();
    pause();
    now
}

// ---------------------------------------------------------------------------
// One write group
// ---------------------------------------------------------------------------

/// `DROP COLUMN` used to write the schema, then one group per row, then the constraint
/// records: a crash or a reader in between saw a schema without the column over rows
/// that still had it. Now the schema, every row's new image (with its version), the
/// constraints naming the column, the missing value and the layout record are one
/// group, on both families.
#[test]
fn drop_column_is_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    for family in FAMILIES {
        let table = format!("dc_{family:?}").to_lowercase();
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, x TEXT, y INT, z TEXT, CONSTRAINT {table}_xu UNIQUE (x), \
             CHECK (y >= 0))"
        ))
        .unwrap();
        let values: Vec<String> = (1..=20).map(|id| format!("({id}, 'x{id}', {id}, 'z{id}')")).collect();
        db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(", ")))
            .unwrap();
        db.execute(&format!("ALTER TABLE {table} ADD COLUMN m INT DEFAULT 7"))
            .unwrap();

        let sql = format!("ALTER TABLE {table} DROP COLUMN x");
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        for key in [
            format!("meta:table:{table}"),
            format!("data:{table}:"),
            format!("v_idx:{table}:"),
            format!("table_constraints:{table}"),
            format!("meta:table_layout:{table}"),
            format!("meta:table_shape:{table}"),
            "wal:entries:".to_string(),
        ] {
            assert!(group.has(&key), "{family:?}: the group lacks {key}: {group:#?}");
        }

        assert_eq!(column_names(&db, &table), vec!["id", "y", "z", "m"], "{family:?}");
        assert_eq!(
            stored_widths(&db, &table),
            vec![4; 20],
            "{family:?}: every row rewritten"
        );
        let read = rows(&db, &format!("SELECT * FROM {table} WHERE id = 3"));
        assert_eq!(
            read,
            vec![vec![Value::Int4(3), Value::Int4(3), s("z3"), Value::Int4(7)]],
            "{family:?}: every column after the dropped one reads its own value"
        );
        // The UNIQUE that named the column went with it, its index too.
        db.execute(&format!("INSERT INTO {table} VALUES (100, 5, 'z', 1)"))
            .unwrap();
        assert!(
            !db.storage
                .art_indexes()
                .list_table_indexes(&table)
                .iter()
                .any(|(_, _, columns)| columns.iter().any(|c| c == "x")),
            "{family:?}: no index over the dropped column is left"
        );
        // The CHECK on another column is kept.
        assert!(db
            .execute(&format!("INSERT INTO {table} VALUES (101, -1, 'z', 1)"))
            .is_err());
    }
}

/// DROP COLUMN removes user index definitions as well as constraint indexes,
/// including when the same statement renamed the dropped column first. Otherwise
/// an old index reappears after reopen or is reused for a new column of that name.
#[test]
fn drop_column_retires_its_user_indexes_in_the_same_group_and_after_reopen() {
    for family in FAMILIES {
        for clause in ["DROP COLUMN x", "RENAME COLUMN x TO xx, DROP COLUMN xx"] {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path());
                db.execute("CREATE TABLE indexed (id INT PRIMARY KEY, x INT, z INT)")
                    .unwrap();
                db.execute("INSERT INTO indexed VALUES (1, 10, 100), (2, 20, 200)")
                    .unwrap();
                db.execute("CREATE INDEX ordinary ON indexed (x)").unwrap();
                db.execute("CREATE UNIQUE INDEX joint ON indexed (z, x)").unwrap();
                let sql = format!("ALTER TABLE indexed {clause}");
                let group = one_group(&db, &sql, || run(&db, family, &sql));
                for name in ["ordinary", "joint"] {
                    assert!(
                        group.has(&format!("meta:index:{name}")),
                        "{family:?}: {clause}: {group:#?}"
                    );
                    assert!(db.storage.catalog().get_index_definition(name).unwrap().is_none());
                    assert!(!db
                        .storage
                        .art_indexes()
                        .list_table_indexes("indexed")
                        .iter()
                        .any(|(index, _, _)| index == name));
                }
                assert_eq!(
                    ints(&rows(&db, "SELECT * FROM indexed ORDER BY id")),
                    vec![vec![1, 100], vec![2, 200]]
                );
            }
            let db = open(dir.path());
            for name in ["ordinary", "joint"] {
                assert!(db.storage.catalog().get_index_definition(name).unwrap().is_none());
            }
            db.execute("ALTER TABLE indexed ADD COLUMN x INT DEFAULT 7").unwrap();
            db.execute("INSERT INTO indexed VALUES (3, 300, 7)").unwrap();
            assert_eq!(
                ints(&rows(&db, "SELECT id FROM indexed WHERE x = 7 ORDER BY id")),
                vec![vec![1], vec![2], vec![3]],
                "{family:?}: {clause}: a new column must not use an old index"
            );
        }
    }
}

/// Every clause of one `ALTER TABLE` in ONE group: an added column with a stable
/// default, a dropped one, a rename, a volatile default, a CHECK over the added column.
#[test]
fn a_multi_clause_alter_is_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    for family in FAMILIES {
        let table = format!("mc_{family:?}").to_lowercase();
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, x INT, y INT, z TEXT)"
        ))
        .unwrap();
        db.execute(&format!(
            "INSERT INTO {table} VALUES (1, 10, 20, 'a'), (2, 11, 21, 'b'), (3, 12, 22, 'c')"
        ))
        .unwrap();
        let sql = format!(
            "ALTER TABLE {table} ADD COLUMN a INT DEFAULT 7, DROP COLUMN x, RENAME COLUMN y TO w, \
             ADD COLUMN u TEXT DEFAULT gen_random_uuid()::text, ADD CONSTRAINT {table}_a_pos CHECK (a > 0)"
        );
        let group = one_group(&db, &sql, || run(&db, family, &sql));
        for key in [
            format!("meta:table:{table}"),
            format!("data:{table}:"),
            format!("table_constraints:{table}"),
            format!("meta:attmissing:{table}:a"),
            format!("meta:table_layout:{table}"),
        ] {
            assert!(group.has(&key), "{family:?}: the group lacks {key}: {group:#?}");
        }
        assert_eq!(column_names(&db, &table), vec!["id", "w", "z", "a", "u"], "{family:?}");
        let read = rows(&db, &format!("SELECT id, w, z, a FROM {table} ORDER BY id"));
        assert_eq!(
            read,
            vec![
                vec![Value::Int4(1), Value::Int4(20), s("a"), Value::Int4(7)],
                vec![Value::Int4(2), Value::Int4(21), s("b"), Value::Int4(7)],
                vec![Value::Int4(3), Value::Int4(22), s("c"), Value::Int4(7)],
            ],
            "{family:?}"
        );
        let uuids = rows(&db, &format!("SELECT u FROM {table}"));
        assert!(uuids.iter().all(|row| row[0] != Value::Null), "{family:?}: {uuids:?}");
        assert_ne!(
            uuids[0], uuids[1],
            "{family:?}: a volatile default gives each row its own"
        );
        assert!(
            db.execute(&format!("INSERT INTO {table} (id, w, z, a) VALUES (9, 1, 'q', 0)"))
                .is_err(),
            "{family:?}: the CHECK over the added column is enforced"
        );
    }
}

/// A refusal by any clause leaves nothing: not the clauses before it (the column an
/// earlier clause added, the one it dropped), not a write group, not an index.
#[test]
fn a_refusal_in_a_later_clause_leaves_the_earlier_clauses_unwritten() {
    for family in FAMILIES {
        for (sql, marker) in [
            (
                "ALTER TABLE r ADD COLUMN a INT DEFAULT 1, DROP COLUMN x, ADD CONSTRAINT r_a CHECK (a > 5)",
                "check constraint",
            ),
            (
                "ALTER TABLE r ADD COLUMN c INT DEFAULT 1 CHECK (c > 5)",
                "check constraint",
            ),
            (
                "ALTER TABLE r DROP COLUMN x, ADD COLUMN q INT, ADD CONSTRAINT r_y UNIQUE (y)",
                "unique constraint",
            ),
            ("ALTER TABLE r DROP COLUMN x, ADD COLUMN y INT", "already exists"),
            (
                "ALTER TABLE r ADD COLUMN price INT, DROP COLUMN nonexistent",
                "nonexistent",
            ),
            (
                "ALTER TABLE r RENAME COLUMN y TO w, ADD COLUMN v INT NOT NULL",
                "violates not-null constraint",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path());
            db.execute("CREATE TABLE r (id INT PRIMARY KEY, x INT, y INT)").unwrap();
            db.execute("INSERT INTO r VALUES (1, 10, 9), (2, 11, 9)").unwrap();
            let before_rows = rows(&db, "SELECT * FROM r ORDER BY id");
            let before = latest(&db);

            let err = attempt(&db, family, sql).expect_err(sql).to_string();
            assert!(err.to_lowercase().contains(marker), "[{family:?}] {sql}: {err}");
            assert!(
                visible_groups(write_groups_since(&db, before)).is_empty(),
                "[{family:?}] {sql}: a refused statement wrote"
            );
            assert_eq!(column_names(&db, "r"), vec!["id", "x", "y"], "[{family:?}] {sql}");
            assert_eq!(
                rows(&db, "SELECT * FROM r ORDER BY id"),
                before_rows,
                "[{family:?}] {sql}"
            );
            assert_eq!(stored_widths(&db, "r"), vec![3, 3], "[{family:?}] {sql}");
            assert!(
                db.storage
                    .art_indexes()
                    .unique_indexes_on_columns("r", &["y".to_string()])
                    .is_empty(),
                "[{family:?}] {sql}: no index is left over y"
            );
            assert!(
                db.storage
                    .catalog()
                    .load_table_constraints("r")
                    .unwrap()
                    .check_constraints
                    .is_empty(),
                "[{family:?}] {sql}: no CHECK record"
            );
            // And the table takes the statement once the data allows it.
            db.execute("ALTER TABLE r ADD COLUMN a INT DEFAULT 6, DROP COLUMN x, ADD CONSTRAINT r_a CHECK (a > 5)")
                .unwrap();
            assert_eq!(column_names(&db, "r"), vec!["id", "y", "a"], "[{family:?}] {sql}");
        }
    }
}

/// `RENAME TO` is a statement of its own, as PostgreSQL has it: combined with other
/// clauses it is refused, and nothing of the other clauses is written.
#[test]
fn rename_to_with_other_clauses_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE rn (id INT PRIMARY KEY, x INT)").unwrap();
    let err = db
        .execute("ALTER TABLE rn ADD COLUMN a INT, RENAME TO rn2")
        .expect_err("refused")
        .to_string();
    assert!(err.contains("RENAME TO"), "{err}");
    assert_eq!(column_names(&db, "rn"), vec!["id", "x"]);
    assert!(db.query("SELECT * FROM rn2", &[]).is_err());
}

/// A parent rename/drop replaces a child's FK record. The child can finish an
/// earlier transaction, including an unrelated CHECK, while the parent waits.
/// Its new CHECK must survive: loading the child's record before taking its lock
/// let the parent later overwrite the CHECK with the stale record.
#[test]
fn inbound_foreign_key_rewrites_read_the_child_record_after_its_writer_finishes() {
    for family in FAMILIES {
        for clause in ["RENAME COLUMN u TO renamed", "DROP COLUMN u CASCADE"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.storage.debug_set_table_lock_timeout(Duration::from_secs(10));
            db.execute("CREATE TABLE z_parent (id INT PRIMARY KEY, u INT UNIQUE)")
                .unwrap();
            db.execute("INSERT INTO z_parent VALUES (1, 10)").unwrap();
            db.execute(
                "CREATE TABLE a_child (id INT PRIMARY KEY, p INT, \
                 CONSTRAINT child_fk FOREIGN KEY (p) REFERENCES z_parent(u))",
            )
            .unwrap();
            db.execute("INSERT INTO a_child VALUES (1, 10)").unwrap();
            let session = db
                .create_session("child holder", heliosdb_nano::session::IsolationLevel::ReadCommitted)
                .unwrap();
            db.begin_transaction_for_session(session).unwrap();
            // No staged rows, but the transaction holds the shared writer lock.
            db.execute_for_session(session, "UPDATE a_child SET p = p WHERE id = 999")
                .unwrap();

            let (child_alter, parent_alter) = std::thread::scope(|scope| {
                let sql = format!("ALTER TABLE z_parent {clause}");
                let database = &db;
                let changing = scope.spawn(move || attempt(database, family, &sql));
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while db.storage.debug_table_lock_waiters("a_child").0 == 0 {
                    assert!(
                        std::time::Instant::now() < deadline && !changing.is_finished(),
                        "{family:?}: {clause}: the parent must wait for the child writer"
                    );
                    std::thread::yield_now();
                }
                let child_alter =
                    db.execute_for_session(session, "ALTER TABLE a_child ADD CONSTRAINT child_kept CHECK (p >= 0)");
                if child_alter.is_ok() {
                    db.commit_transaction_for_session(session).unwrap();
                } else {
                    db.rollback_transaction_for_session(session).unwrap();
                }
                (child_alter, changing.join().unwrap())
            });
            child_alter.unwrap();
            parent_alter.unwrap();
            let constraints = db.storage.catalog().load_table_constraints("a_child").unwrap();
            assert!(
                constraints
                    .check_constraints
                    .iter()
                    .any(|check| check.name == "child_kept"),
                "{family:?}: {clause}: parent overwrote the child's concurrently committed CHECK"
            );
            if clause.starts_with("RENAME") {
                assert_eq!(constraints.foreign_keys.len(), 1);
                assert_eq!(constraints.foreign_keys[0].references_columns, vec!["renamed"]);
            } else {
                assert!(constraints.foreign_keys.is_empty());
            }
            db.destroy_session(session).unwrap();
        }
    }
}

/// `SET STORAGE` moved each value to its new representation row by row, a group each,
/// and wrote the schema after: a reader or a crash between them saw dictionary
/// references under a schema that said plain values, or the reverse. One group now,
/// whatever the move.
#[test]
fn set_storage_is_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.execute("CREATE TABLE ss (id INT PRIMARY KEY, tag TEXT, n INT)")
        .unwrap();
    let values: Vec<String> = (1..=30).map(|id| format!("({id}, 'tag{}', {id})", id % 3)).collect();
    db.execute(&format!("INSERT INTO ss VALUES {}", values.join(", ")))
        .unwrap();
    let want = rows(&db, "SELECT id, tag, n FROM ss ORDER BY id");
    for (mode, family_key) in [
        ("DICTIONARY", Some("dict:ss:tag")),
        ("COLUMNAR", Some("col:ss:tag:")),
        ("DEFAULT", None),
    ] {
        let sql = format!("ALTER TABLE ss ALTER COLUMN tag SET STORAGE {mode}");
        let group = one_group(&db, &sql, || db.execute(&sql).map(drop).unwrap());
        assert!(group.has("meta:table:ss"), "{mode}: {group:#?}");
        assert!(group.has("data:ss:"), "{mode}: {group:#?}");
        if let Some(key) = family_key {
            assert!(group.has(key), "{mode}: the group lacks {key}: {group:#?}");
        }
        assert_eq!(rows(&db, "SELECT id, tag, n FROM ss ORDER BY id"), want, "{mode}");
        assert_eq!(
            rows(&db, "SELECT n FROM ss WHERE tag = 'tag1' ORDER BY n").len(),
            10,
            "{mode}"
        );
    }
}

/// A `DROP COLUMN` whose one group would pass `[storage] max_atomic_rewrite_bytes` is
/// refused (54000) with nothing written, naming the alternative.
#[test]
fn drop_column_above_the_cap_is_refused_and_writes_nothing() {
    let build = |rows: usize| {
        let mut config = Config::in_memory();
        config.storage.max_atomic_rewrite_bytes = 1024 * 1024;
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE big (id INT PRIMARY KEY, body TEXT, n INT)")
            .unwrap();
        let body = "b".repeat(1000);
        for start in (0..rows).step_by(200) {
            let values: Vec<String> = (start..(start + 200).min(rows))
                .map(|id| format!("({id}, '{body}', {id})"))
                .collect();
            db.execute(&format!("INSERT INTO big VALUES {}", values.join(", ")))
                .unwrap();
        }
        db
    };
    for family in FAMILIES {
        let db = build(2000);
        let err = attempt(&db, family, "ALTER TABLE big DROP COLUMN n")
            .expect_err("above the cap")
            .to_string();
        assert!(
            err.contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER),
            "[{family:?}] {err}"
        );
        assert!(
            err.contains("CREATE TABLE"),
            "[{family:?}] names the alternative: {err}"
        );
        assert_eq!(column_names(&db, "big"), vec!["id", "body", "n"], "[{family:?}]");
        assert_eq!(stored_widths(&db, "big"), vec![3; 2000], "[{family:?}]");

        let small = build(100);
        run(&small, family, "ALTER TABLE big DROP COLUMN body");
        assert_eq!(stored_widths(&small, "big"), vec![2; 100], "[{family:?}]");
        assert_eq!(int(&rows(&small, "SELECT n FROM big WHERE id = 42")[0][0]), 42);
    }
}

// ---------------------------------------------------------------------------
// Reads older than a change of the layout
// ---------------------------------------------------------------------------

/// Review P3. A version written before a `DROP COLUMN` keeps the dropped value in place;
/// read by position under today's schema it showed that value under the next column's
/// name, in `AS OF` and in every transaction whose snapshot was older. It is read under
/// the layout it was written with now, then shown in today's columns.
#[test]
fn as_of_before_a_drop_column_reads_each_value_under_its_own_column() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY, x INT, y INT)").unwrap();
    db.execute("INSERT INTO p VALUES (1, 10, 20), (2, 11, 21)").unwrap();
    db.execute("CREATE TABLE q (id INT PRIMARY KEY, secret TEXT)").unwrap();
    db.execute("INSERT INTO q VALUES (1, 's1')").unwrap();
    let before = mark();
    db.execute("ALTER TABLE p DROP COLUMN x").unwrap();
    db.execute("UPDATE p SET y = 22 WHERE id = 1").unwrap();
    db.execute("ALTER TABLE q DROP COLUMN secret").unwrap();
    db.execute("ALTER TABLE q ADD COLUMN pub TEXT DEFAULT 'public'")
        .unwrap();
    let after = mark();

    let sorted = |sql: String| {
        let mut read = ints(&rows(&db, &sql));
        read.sort();
        read
    };
    assert_eq!(
        sorted(format!("SELECT * FROM p AS OF TIMESTAMP '{before}'")),
        vec![vec![1, 20], vec![2, 21]],
        "before the DROP: y reads y's value, never x's"
    );
    assert_eq!(
        sorted(format!("SELECT y FROM p AS OF TIMESTAMP '{before}' WHERE id = 2")),
        vec![vec![21]]
    );
    assert_eq!(
        sorted(format!("SELECT * FROM p AS OF TIMESTAMP '{after}'")),
        vec![vec![1, 22], vec![2, 21]]
    );
    // The dropped column's value is never read under a later column's name.
    assert_eq!(
        rows(&db, &format!("SELECT * FROM q AS OF TIMESTAMP '{before}'")),
        vec![vec![Value::Int4(1), s("public")]],
        "a version older than `pub` reads its missing value, not the dropped secret"
    );

    // Every version, each in today's layout.
    let versions = rows(
        &db,
        &format!("SELECT * FROM p VERSIONS BETWEEN TIMESTAMP '{before}' AND TIMESTAMP '{after}'"),
    );
    for version in &versions {
        assert_eq!(version.len(), 2, "{versions:?}");
        assert!(
            [20, 21, 22].contains(&int(&version[1])),
            "a shifted version: {versions:?}"
        );
    }
}

/// A REPEATABLE READ transaction begun before a `DROP COLUMN` reads its snapshot after
/// it, in the scan and in the point read, each value under its own column.
#[test]
fn a_transaction_begun_before_a_drop_column_reads_each_value_under_its_own_column() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY, x INT, y INT, z TEXT)")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1, 10, 20, 'a'), (2, 11, 21, 'b')")
        .unwrap();
    let session = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    assert_eq!(
        db.query_in_session(session, "SELECT id FROM p", &[]).unwrap().len(),
        2,
        "vacuity: its snapshot holds the rows"
    );

    db.execute("ALTER TABLE p DROP COLUMN x").unwrap();
    db.execute("UPDATE p SET y = 99 WHERE id = 1").unwrap();

    let mut read: Vec<Vec<Value>> = db
        .query_in_session(session, "SELECT * FROM p", &[])
        .unwrap()
        .into_iter()
        .map(|t| t.values)
        .collect();
    read.sort_by_key(|row| int(&row[0]));
    assert_eq!(
        read,
        vec![
            vec![Value::Int4(1), Value::Int4(20), s("a")],
            vec![Value::Int4(2), Value::Int4(21), s("b")],
        ],
        "its snapshot, in today's columns"
    );
    let point: Vec<Vec<Value>> = db
        .query_in_session(session, "SELECT y, z FROM p WHERE id = 2", &[])
        .unwrap()
        .into_iter()
        .map(|t| t.values)
        .collect();
    assert_eq!(point, vec![vec![Value::Int4(21), s("b")]], "the point read");
    db.commit_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
}

/// The transaction that drops the column reads the committed rows it did not touch
/// right, at its own older snapshot.
#[test]
fn a_transaction_reads_its_own_drop_column_right() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE o (id INT PRIMARY KEY, mid TEXT, last INT)")
        .unwrap();
    db.execute("INSERT INTO o VALUES (1, 'm1', 10), (2, 'm2', 20)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("ALTER TABLE o DROP COLUMN mid").unwrap();
    let want = vec![vec![1, 10], vec![2, 20]];
    assert_eq!(ints(&rows(&db, "SELECT * FROM o ORDER BY id")), want);
    assert_eq!(ints(&rows(&db, "SELECT last FROM o WHERE id = 2")), vec![vec![20]]);
    db.execute("COMMIT").unwrap();
    assert_eq!(ints(&rows(&db, "SELECT * FROM o ORDER BY id")), want);
}

/// `RENAME COLUMN` moves no position: a version older than it reads the value under the
/// column's new name.
#[test]
fn as_of_before_a_rename_column_reads_the_value_under_its_new_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE rc (id INT PRIMARY KEY, y INT, z INT)")
        .unwrap();
    db.execute("INSERT INTO rc VALUES (1, 20, 30)").unwrap();
    let session = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    assert_eq!(db.query_in_session(session, "SELECT * FROM rc", &[]).unwrap().len(), 1);
    let before = mark();
    db.execute("ALTER TABLE rc RENAME COLUMN y TO w").unwrap();
    db.execute("UPDATE rc SET w = 21 WHERE id = 1").unwrap();
    pause();
    assert_eq!(
        ints(&rows(&db, &format!("SELECT w, z FROM rc AS OF TIMESTAMP '{before}'"))),
        vec![vec![20, 30]]
    );
    let read: Vec<Vec<Value>> = db
        .query_in_session(session, "SELECT w, z FROM rc WHERE id = 1", &[])
        .unwrap()
        .into_iter()
        .map(|t| t.values)
        .collect();
    assert_eq!(
        ints(&read),
        vec![vec![20, 30]],
        "REPEATABLE READ begun before the rename"
    );
    db.commit_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
}

/// A renamed table's history stayed with its old name, which purges it: before the
/// rename the new name holds nothing right to read. Refused (0A000), for `AS OF` and for
/// a transaction whose snapshot is older; the transaction that renamed it reads it.
#[test]
fn a_read_before_a_rename_to_is_refused_unless_the_transaction_renamed_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ra (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO ra VALUES (1, 10), (2, 20)").unwrap();
    let session = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    assert_eq!(db.query_in_session(session, "SELECT * FROM ra", &[]).unwrap().len(), 2);
    let before = mark();
    db.execute("ALTER TABLE ra RENAME TO rb").unwrap();
    // Nothing commits after the rename: `after` reads at the rename's own `snapshot:`
    // record. Without one it read at the INSERT's, before the rename, and was refused.
    let after = mark();

    let err = db
        .query(&format!("SELECT * FROM rb AS OF TIMESTAMP '{before}'"), &[])
        .expect_err("refused")
        .to_string();
    assert!(err.contains(BRANCH_ANCHOR_UNREADABLE_MARKER), "{err}");
    assert!(err.contains("rb"), "names the table: {err}");
    assert_eq!(
        ints(&rows(
            &db,
            &format!("SELECT * FROM rb AS OF TIMESTAMP '{after}' ORDER BY id")
        )),
        vec![vec![1, 10], vec![2, 20]]
    );

    let err = db
        .query_in_session(session, "SELECT * FROM rb", &[])
        .expect_err("its snapshot is older than the rename")
        .to_string();
    assert!(err.contains(BRANCH_ANCHOR_UNREADABLE_MARKER), "{err}");
    db.rollback_transaction_for_session(session).ok();
    db.destroy_session(session).unwrap();

    // Its own rename: the transaction reads what it renamed.
    db.execute("BEGIN").unwrap();
    db.execute("ALTER TABLE rb RENAME TO rc").unwrap();
    assert_eq!(
        ints(&rows(&db, "SELECT * FROM rc ORDER BY id")),
        vec![vec![1, 10], vec![2, 20]]
    );
    db.execute("COMMIT").unwrap();
}

/// A refresh replaces a view's rows. Read at a time before it, the view is what it was,
/// or the read is refused; never a mix and never the new rows.
#[test]
fn as_of_before_a_refresh_reads_the_view_as_it_was_or_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, g INT)").unwrap();
    db.execute("INSERT INTO src VALUES (1, 1), (2, 2)").unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, g FROM src")
        .unwrap();
    let session = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    assert_eq!(db.query_in_session(session, "SELECT * FROM mv", &[]).unwrap().len(), 2);
    let before = mark();
    db.execute("INSERT INTO src VALUES (3, 3)").unwrap();
    db.execute("UPDATE src SET g = 9 WHERE id = 1").unwrap();
    db.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
    pause();
    let as_it_was = |answer: heliosdb_nano::Result<Vec<heliosdb_nano::Tuple>>, what: &str| match answer {
        Ok(read) => {
            let mut read = ints(&read.into_iter().map(|t| t.values).collect::<Vec<_>>());
            read.sort();
            assert_eq!(read, vec![vec![1, 1], vec![2, 2]], "{what}: the view as it was");
        }
        Err(e) => assert!(e.to_string().contains(BRANCH_ANCHOR_UNREADABLE_MARKER), "{what}: {e}"),
    };
    as_it_was(
        db.query(&format!("SELECT id, g FROM mv AS OF TIMESTAMP '{before}'"), &[]),
        "AS OF",
    );
    // A transaction's snapshot scan leaves out a row deleted since its snapshot (what it
    // has always read: `StorageEngine::scan_table_at_transaction_snapshot`), and a refresh
    // deletes the view's rows: what it reads is the view as it was, less those, or a
    // refusal. Never a row of the refresh.
    match db.query_in_session(session, "SELECT id, g FROM mv", &[]) {
        Ok(read) => {
            let read = ints(&read.into_iter().map(|t| t.values).collect::<Vec<_>>());
            assert!(
                read.iter().all(|row| [vec![1, 1], vec![2, 2]].contains(row)),
                "REPEATABLE READ begun before the refresh read a row of it: {read:?}"
            );
        }
        Err(e) => assert!(e.to_string().contains(BRANCH_ANCHOR_UNREADABLE_MARKER), "{e}"),
    }
    db.rollback_transaction_for_session(session).ok();
    db.destroy_session(session).unwrap();
}

/// Storage migrations preserve logical historical values across the old and new
/// representations; neither answer may expose a dictionary reference.
#[test]
fn as_of_before_and_after_a_move_out_of_dictionary_storage_keeps_logical_values() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ds (id INT PRIMARY KEY, tag TEXT)").unwrap();
    db.execute("INSERT INTO ds VALUES (1, 'a'), (2, 'b')").unwrap();
    db.execute("ALTER TABLE ds ALTER COLUMN tag SET STORAGE DICTIONARY")
        .unwrap();
    let before = mark();
    db.execute("ALTER TABLE ds ALTER COLUMN tag SET STORAGE DEFAULT")
        .unwrap();
    let after = mark();
    let mut old = rows(&db, &format!("SELECT id, tag FROM ds AS OF TIMESTAMP '{before}'"));
    old.sort_by_key(|row| int(&row[0]));
    assert_eq!(old, vec![vec![Value::Int4(1), s("a")], vec![Value::Int4(2), s("b")]]);
    let mut read = rows(&db, &format!("SELECT id, tag FROM ds AS OF TIMESTAMP '{after}'"));
    read.sort_by_key(|row| int(&row[0]));
    assert_eq!(read, vec![vec![Value::Int4(1), s("a")], vec![Value::Int4(2), s("b")]]);
}

/// A reader during `DROP COLUMN` sees the table before or after it, never a value under
/// the wrong column. One group makes every crash state and every row set one or the
/// other; what is left is a reader that resolved the schema before the group and reads
/// the rows after it, which spec R5.6's layout epochs close.
#[test]
fn a_reader_never_sees_a_shifted_value_during_drop_column() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let shapes = Arc::new(AtomicUsize::new(0));
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let _cleanup = StopOnDrop(Arc::clone(&stop));
    let reader = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let shapes = Arc::clone(&shapes);
        std::thread::spawn(move || {
            let mut reads = 0usize;
            while !stop.load(Ordering::Acquire) {
                match db.query("SELECT * FROM rl ORDER BY id", &[]) {
                    Ok(answer) => {
                        if answer.is_empty() {
                            continue;
                        }
                        assert_eq!(answer.len(), 200, "partial DROP window row set");
                        let mut observed = 0;
                        for (index, row) in answer.into_iter().enumerate() {
                            let values = &row.values;
                            let id = int(&values[0]);
                            assert_eq!(id, i64::try_from(index + 1).unwrap(), "missing or duplicate id");
                            match values.len() {
                                3 => {
                                    assert_eq!(int(&values[1]), 10 * id, "before: {values:?}");
                                    assert_eq!(int(&values[2]), 20 * id, "before: {values:?}");
                                    observed |= 1;
                                }
                                2 => {
                                    assert_eq!(int(&values[1]), 20 * id, "after: {values:?}");
                                    observed |= 2;
                                }
                                _ => panic!("a row of neither shape: {values:?}"),
                            }
                        }
                        assert!(observed == 1 || observed == 2, "one answer mixed row layouts");
                        shapes.fetch_or(observed, Ordering::Release);
                        reads += 1;
                    }
                    Err(error) => {
                        let absent = matches!(&error, heliosdb_nano::Error::QueryExecution(message)
                            if matches!(message.as_str(), "Table 'rl' not found" | "Table 'rl' does not exist" | "Table rl not found"));
                        assert!(
                            absent || heliosdb_nano::storage::layout_epochs::is_layout_changed(&error),
                            "unexpected DROP window error: {error}"
                        );
                    }
                }
            }
            reads
        })
    };
    let observe = |mask| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while shapes.load(Ordering::Acquire) & mask == 0 {
            assert!(
                Instant::now() < deadline,
                "reader did not observe nonempty shape {mask}"
            );
            std::thread::yield_now();
        }
    };
    for round in 0..20 {
        db.execute("CREATE TABLE rl (id INT PRIMARY KEY, x INT, y INT)")
            .unwrap();
        let values: Vec<String> = (1..=200)
            .map(|id| format!("({id}, {}, {})", 10 * id, 20 * id))
            .collect();
        db.execute(&format!("INSERT INTO rl VALUES {}", values.join(", ")))
            .unwrap();
        if round == 0 {
            observe(1);
        }
        db.execute("ALTER TABLE rl DROP COLUMN x").unwrap();
        if round == 0 {
            observe(2);
        }
        db.execute("DROP TABLE rl").unwrap();
    }
    stop.store(true, Ordering::Release);
    let reads = reader.join().expect("the reader saw a shifted value");
    assert!(reads > 0, "vacuity: no nonempty read");
    assert_eq!(shapes.load(Ordering::Acquire), 3, "both nonempty layouts were observed");
}

// ---------------------------------------------------------------------------
// Crash
// ---------------------------------------------------------------------------

/// Set by the parent on its re-exec of this test binary: the directory the crash child
/// works on.
const CRASH_DIR_ENV: &str = "HELIOS_R5_5_CRASH_DIR";

/// Rows below the checkpoint (a clean close), then `DROP COLUMN` and an exit with no
/// close: the reopened table has each value in its own column, the schema and the
/// layout the DROP left. (Rows logged above the checkpoint are spec R5.7's: the replay
/// at open puts their logged images back, see `tests/row_shape.rs`.)
#[test]
fn a_crash_right_after_drop_column_keeps_each_value_in_its_column() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = open(Path::new(&dir));
        db.execute("ALTER TABLE cr DROP COLUMN x").unwrap();
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        db.execute("CREATE TABLE cr (id INT PRIMARY KEY, x INT, y INT)")
            .unwrap();
        db.execute("INSERT INTO cr VALUES (1, 10, 20), (2, 11, 21)").unwrap();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("a_crash_right_after_drop_column_keeps_each_value_in_its_column")
        .arg("--nocapture")
        .env(CRASH_DIR_ENV, dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "the crash child failed: {status:?}");
    let db = open(dir.path());
    assert_eq!(column_names(&db, "cr"), vec!["id", "y"]);
    assert_eq!(
        ints(&rows(&db, "SELECT * FROM cr ORDER BY id")),
        vec![vec![1, 20], vec![2, 21]]
    );
    assert_eq!(ints(&rows(&db, "SELECT y FROM cr WHERE id = 2")), vec![vec![21]]);
    assert!(db.storage.db().get(b"meta:table_layout:cr").unwrap().is_some());
}
