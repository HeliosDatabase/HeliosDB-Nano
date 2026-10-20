//! Tripwire B (spec P0.1): nothing a primary writes is `Unknown` to
//! `storage::keyspace::classify`.
//!
//! Tripwire A (`keyspace_classification.rs`) proves every key-shaped literal
//! in `src/` is classified, but it cannot see a key assembled from parts it
//! does not recognise as a key. This test looks at what a real primary
//! actually WROTE: one persistent `EmbeddedDatabase` runs a corpus touching
//! every subsystem, closes cleanly (index snapshots, row counters and the WAL
//! checkpoint flush), reopens (open-time writes: the temp-table sweep, counter
//! reseed, WAL truncation) for a few more statements, and closes again. The
//! RocksDB WAL is then read from sequence 0, and the live keyspace walked,
//! through a read-only handle, and every key is classified.
//!
//! # Reading everything that was written
//!
//! * `replication.physical.enabled = true` sets RocksDB's WAL TTL, so WAL files
//!   obsoleted by memtable flushes are archived rather than deleted and
//!   `get_updates_since(0)` replays the whole history. Nothing else in the
//!   embedded open path reads that flag.
//! * `WriteBatch::iterate` reports puts and deletes only: range deletions and
//!   single deletes are dropped silently, and merges or column-family records
//!   would stop the iteration without an error. Every batch is therefore ALSO
//!   decoded from its raw bytes (the RocksDB 8.10 format that
//!   `replication::physical::validate_raw_batch` decodes): range deletions and
//!   single deletes are classified from that decode, an opcode neither decoder
//!   understands fails the test, the two decoders must agree on every put and
//!   delete, and the decoded op count must equal the batch header's.
//! * The walk must be gap-free from sequence 1 to `latest_sequence_number`, and
//!   the prefix-less iteration of the live keyspace covers what survives.
//!
//! # Non-vacuity
//!
//! * The observed classes must include every class the corpus is known to
//!   produce ([`REQUIRED_CLASSES`]); a statement that silently stops writing
//!   fails here, by class. Each corpus step also asserts its own effect.
//! * Red proof, on every run: the same keys classified with `snapshot:` removed
//!   from the classifier must report snapshot keys and nothing else.
//!
//! # COPY
//!
//! `COPY … FROM STDIN` is reachable only through the PostgreSQL wire protocol
//! (`EmbeddedDatabase::copy_bulk_insert` is `pub(crate)`). Its storage write is
//! `StorageEngine::insert_prepared_tuples_fast_batch`, which with time travel
//! on writes the rows plus one `vmeta:` COPY marker. The only other caller is
//! the batch fast path of `EmbeddedDatabase::execute_many_params`, which the
//! corpus uses, so COPY's keys are covered without a server. (The SQL
//! multi-row `INSERT … VALUES` arm does not reach that call; it is run too, for
//! its own per-row version keys.)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::keyspace::classify;
use heliosdb_nano::storage::KeyClass;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::{IteratorMode, Options, WriteBatchIterator, DB};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Classes the corpus below must produce. Each is tied to the statement that
/// writes it, so a miss names a statement that stopped writing.
const REQUIRED_CLASSES: &[KeyClass] = &[
    KeyClass::Data,
    KeyClass::Version,
    KeyClass::VersionIdx,
    KeyClass::Snapshot,
    // `execute_many_params` (COPY's storage call) with time travel on.
    KeyClass::Vmeta,
    KeyClass::Counter,
    KeyClass::MetaTable,
    KeyClass::MetaIndex,
    KeyClass::MetaSchema,
    KeyClass::MetaEnumType,
    KeyClass::MetaIdentity,
    KeyClass::MetaPartition,
    KeyClass::MetaSequence,
    KeyClass::MetaSeqState,
    KeyClass::MetaMv,
    KeyClass::MetaRole,
    KeyClass::MetaAcl,
    KeyClass::MetaFunction,
    KeyClass::MetaProcedure,
    KeyClass::TableConstraints,
    KeyClass::Trigger,
    KeyClass::TriggerRowMut,
    KeyClass::Statistics,
    // DROP TABLE deletes the table's compression records.
    KeyClass::Compression,
    KeyClass::ViewMeta,
    KeyClass::Dict,
    KeyClass::Columnar,
    KeyClass::Cas,
    KeyClass::Branch,
    KeyClass::BranchData,
    // Clean close: durable index snapshots.
    KeyClass::IdxSnap,
    KeyClass::IdxSnapMarker,
    KeyClass::VecSnap,
    KeyClass::VecSnapMarker,
    // DDL is logged to the logical WAL by default; a clean close checkpoints it.
    KeyClass::WalEntries,
    KeyClass::WalLastLsn,
    KeyClass::WalCheckpoint,
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn config(dir: &Path) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    // Already the default for a persistent store (`StorageConfig::default`),
    // restated because the logical-WAL keys are part of what is classified.
    c.storage.wal_enabled = true;
    // Archive obsolete RocksDB WAL files instead of deleting them, so the whole
    // write history is readable from sequence 0 (see the module docs).
    c.replication.physical.enabled = true;
    c
}

/// Open (or reopen) the store. Retried: a just-dropped handle's background
/// threads release the RocksDB LOCK asynchronously (as in `gh_issue_35.rs`).
fn open(dir: &Path) -> EmbeddedDatabase {
    let mut last_err = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config(dir)) {
            Ok(db) => return db,
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("opening {} failed: {last_err:?}", dir.display());
}

fn run(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("corpus statement failed: {sql}\n  {e}"));
}

/// The first column of every row, as an integer.
fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("corpus query failed: {sql}\n  {e}"))
        .iter()
        .map(|row| match row.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("{sql}: expected an integer column, got {other:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Session one: one statement family per subsystem, each checking its own
/// effect where a silent no-op would otherwise go unnoticed.
fn session_one(db: &EmbeddedDatabase) {
    // Catalog side records.
    run(db, "CREATE SCHEMA ks_app");
    run(db, "CREATE TYPE ks_mood AS ENUM ('calm', 'busy')");
    run(db, "CREATE ROLE ks_reader");

    // Tables with PRIMARY KEY, UNIQUE and FOREIGN KEY constraints.
    run(db, "CREATE TABLE ks_parent (id INT PRIMARY KEY, name TEXT UNIQUE)");
    run(
        db,
        "CREATE TABLE ks_child (id INT PRIMARY KEY, parent_id INT REFERENCES ks_parent(id), note TEXT)",
    );
    run(db, "GRANT SELECT ON ks_parent TO ks_reader");
    run(
        db,
        "CREATE TABLE ks_ident (id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT)",
    );
    run(
        db,
        "CREATE TABLE ks_range (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)",
    );
    run(
        db,
        "CREATE TABLE ks_range_low PARTITION OF ks_range FOR VALUES FROM (0) TO (100)",
    );
    run(db, "CREATE TEMPORARY TABLE ks_temp (id INT)");
    run(db, "INSERT INTO ks_temp VALUES (1)");

    // Autocommit DML (`data:`, `v:`, `v_idx:`, `snapshot:`, `counter:`).
    for (id, name) in [(1, "one"), (2, "two"), (3, "three"), (4, "four")] {
        run(db, &format!("INSERT INTO ks_parent VALUES ({id}, '{name}')"));
    }
    run(db, "UPDATE ks_parent SET name = 'uno' WHERE id = 1");
    run(db, "DELETE FROM ks_parent WHERE id = 4");
    run(db, "INSERT INTO ks_child VALUES (10, 1, 'first')");
    run(db, "INSERT INTO ks_ident (v) VALUES ('x')");
    run(db, "INSERT INTO ks_range_low (id, label) VALUES (5, 'five')");

    // COPY's storage call (see the module docs): the batch INSERT entry point
    // that shares it and its `vmeta:` marker, then the SQL multi-row arm.
    run(db, "CREATE TABLE ks_copy (id INT PRIMARY KEY, note TEXT)");
    let rows: Vec<Vec<Value>> = (1..=3)
        .map(|id| vec![Value::Int4(id), Value::String(format!("row-{id}"))])
        .collect();
    db.execute_many_params("INSERT INTO ks_copy (id, note) VALUES ($1, $2)", &rows)
        .expect("batch INSERT through execute_many_params");
    run(db, "INSERT INTO ks_copy VALUES (4, 'd'), (5, 'e')");
    assert_eq!(ints(db, "SELECT id FROM ks_copy ORDER BY id"), vec![1, 2, 3, 4, 5]);

    // Per-column storage sidecars (`col:`, `dict:`, `cas:`).
    run(
        db,
        "CREATE TABLE ks_columnar (id INT PRIMARY KEY, tag TEXT STORAGE COLUMNAR, note TEXT)",
    );
    run(db, "INSERT INTO ks_columnar VALUES (1, 'tag-a', 'n')");
    // `dict:` and `cas:` are written by the storage-mode MIGRATION
    // (`StorageEngine::migrate_column_storage`, reached through
    // `ALTER TABLE … SET STORAGE …` on a populated table) and by inserts made
    // after it; an inline `STORAGE …` clause on CREATE TABLE does not produce
    // them on its own (see tests/storage_modes_test.rs for the same sequence).
    run(db, "CREATE TABLE ks_dict (id INT PRIMARY KEY, status TEXT, note TEXT)");
    run(db, "INSERT INTO ks_dict VALUES (1, 'open', 'a')");
    run(db, "INSERT INTO ks_dict VALUES (2, 'open', 'b')");
    run(db, "ALTER TABLE ks_dict ALTER COLUMN status SET STORAGE DICTIONARY");
    run(db, "INSERT INTO ks_dict VALUES (3, 'closed', 'c')");
    run(db, "CREATE TABLE ks_cas (id INT PRIMARY KEY, body TEXT, note TEXT)");
    // Over `content_addr::CAS_MIN_SIZE` (1 KiB), so the payload goes to `cas:`.
    run(
        db,
        &format!("INSERT INTO ks_cas VALUES (1, '{}', 'n')", "x".repeat(1200)),
    );
    run(db, "ALTER TABLE ks_cas ALTER COLUMN body SET STORAGE CONTENT_ADDRESSED");
    run(
        db,
        &format!("INSERT INTO ks_cas VALUES (2, '{}', 'n')", "y".repeat(1200)),
    );

    // Indexes: B-tree and HNSW (persistent HNSW too when it is compiled in).
    run(db, "CREATE INDEX ks_parent_name_idx ON ks_parent (name)");
    run(db, "CREATE TABLE ks_vec (id INT PRIMARY KEY, emb VECTOR(3))");
    run(
        db,
        "INSERT INTO ks_vec VALUES (1, CAST('[1.0,0.0,0.0]' AS VECTOR(3))), \
         (2, CAST('[0.0,1.0,0.0]' AS VECTOR(3))), (3, CAST('[0.0,0.0,1.0]' AS VECTOR(3)))",
    );
    run(db, "CREATE INDEX ks_vec_emb_idx ON ks_vec USING hnsw (emb)");
    assert_eq!(
        ints(db, "SELECT id FROM ks_vec ORDER BY emb <-> '[1.0, 0.0, 0.0]' LIMIT 1"),
        vec![1]
    );
    if cfg!(feature = "vector-persist") {
        run(db, "CREATE TABLE ks_pvec (id INT PRIMARY KEY, emb VECTOR(3))");
        run(db, "INSERT INTO ks_pvec VALUES (1, CAST('[1.0,0.0,0.0]' AS VECTOR(3)))");
        run(
            db,
            "CREATE INDEX ks_pvec_emb_idx ON ks_pvec USING hnsw (emb) WITH (persistent = true)",
        );
    }

    // Sequences.
    run(db, "CREATE SEQUENCE ks_seq START 10");
    assert_eq!(ints(db, "SELECT nextval('ks_seq')"), vec![10]);

    // Views and a materialized view refreshed after a base-table change.
    run(db, "CREATE VIEW ks_view AS SELECT id, name FROM ks_parent");
    run(db, "CREATE MATERIALIZED VIEW ks_mv AS SELECT id, name FROM ks_parent");
    run(db, "INSERT INTO ks_parent VALUES (5, 'five')");
    run(db, "REFRESH MATERIALIZED VIEW ks_mv");
    assert!(
        ints(db, "SELECT id FROM ks_mv ORDER BY id").contains(&5),
        "REFRESH MATERIALIZED VIEW must materialize the row inserted after CREATE"
    );

    // Routines and a BEFORE-row rewrite trigger (`trigger:`, `trigger_rowmut:`).
    run(
        db,
        "CREATE FUNCTION ks_double(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
    );
    assert_eq!(ints(db, "SELECT ks_double(21)"), vec![42]);
    run(
        db,
        "CREATE FUNCTION ks_rewrite() RETURNS TRIGGER AS $$ BEGIN NEW.note = 'rewritten'; RETURN NEW; END $$ \
         LANGUAGE plpgsql",
    );
    run(
        db,
        "CREATE TRIGGER ks_trigger BEFORE INSERT ON ks_child FOR EACH ROW EXECUTE FUNCTION ks_rewrite()",
    );
    run(
        db,
        "CREATE PROCEDURE ks_proc() LANGUAGE sql AS $$INSERT INTO ks_child VALUES (11, 2, 'proc')$$",
    );
    run(db, "CALL ks_proc()");
    assert_eq!(ints(db, "SELECT id FROM ks_child WHERE id = 11"), vec![11]);

    // Branch: create, write on it (`bdata:` / `bdel:` / `bv:`), merge back.
    run(db, "CREATE DATABASE BRANCH ks_dev FROM main AS OF NOW");
    run(db, "USE BRANCH ks_dev");
    run(db, "INSERT INTO ks_parent VALUES (6, 'six')");
    run(db, "UPDATE ks_parent SET name = 'deux' WHERE id = 2");
    run(db, "DELETE FROM ks_parent WHERE id = 3");
    run(db, "USE BRANCH main");
    assert!(
        !ints(db, "SELECT id FROM ks_parent").contains(&6),
        "main must not see the branch row before the merge"
    );
    run(db, "MERGE DATABASE BRANCH ks_dev INTO main");
    assert!(
        ints(db, "SELECT id FROM ks_parent").contains(&6),
        "the branch row must be visible on main after MERGE"
    );

    // TRUNCATE, ALTER TABLE ADD COLUMN, DROP TABLE (with an index).
    run(db, "CREATE TABLE ks_trunc (id INT PRIMARY KEY)");
    run(db, "INSERT INTO ks_trunc VALUES (1)");
    run(db, "INSERT INTO ks_trunc VALUES (2)");
    run(db, "TRUNCATE ks_trunc");
    assert!(ints(db, "SELECT id FROM ks_trunc").is_empty());
    run(db, "ALTER TABLE ks_parent ADD COLUMN extra TEXT");
    run(db, "CREATE TABLE ks_drop (id INT PRIMARY KEY, v TEXT)");
    run(db, "INSERT INTO ks_drop VALUES (1, 'gone')");
    run(db, "CREATE INDEX ks_drop_v_idx ON ks_drop (v)");
    run(db, "DROP TABLE ks_drop");

    // An explicit transaction with a savepoint rolled back.
    run(db, "CREATE TABLE ks_txn (id INT PRIMARY KEY, note TEXT)");
    run(db, "BEGIN");
    run(db, "INSERT INTO ks_txn VALUES (1, 'kept')");
    run(db, "SAVEPOINT ks_sp");
    run(db, "INSERT INTO ks_txn VALUES (2, 'rolled back')");
    run(db, "ROLLBACK TO SAVEPOINT ks_sp");
    run(db, "INSERT INTO ks_txn VALUES (3, 'kept too')");
    run(db, "COMMIT");
    assert_eq!(ints(db, "SELECT id FROM ks_txn ORDER BY id"), vec![1, 3]);

    // A time-travel read over an updated row, and ANALYZE statistics
    // (`statistics:table:`).
    run(db, "CREATE TABLE ks_tt (id INTEGER PRIMARY KEY, v INTEGER)");
    run(db, "INSERT INTO ks_tt (id, v) VALUES (1, 10)");
    run(db, "UPDATE ks_tt SET v = 20 WHERE id = 1");
    assert_eq!(
        ints(
            db,
            "SELECT id FROM ks_tt AS OF TIMESTAMP '2999-01-01 00:00:00' WHERE id = 1"
        ),
        vec![1]
    );
    db.storage.analyze_table("ks_parent").expect("analyze_table(ks_parent)");
}

/// Session two, after a clean close: whatever the open path writes, plus the
/// first mutation after reopen (it invalidates the `idxsnapv:` markers) and a
/// branch dropped outright.
fn session_two(db: &EmbeddedDatabase) {
    run(db, "INSERT INTO ks_parent (id, name) VALUES (7, 'seven')");
    assert!(ints(db, "SELECT id FROM ks_parent").contains(&7));
    run(db, "CREATE DATABASE BRANCH ks_scratch FROM main AS OF NOW");
    run(db, "DROP DATABASE BRANCH ks_scratch");
    assert_eq!(ints(db, "SELECT id FROM ks_copy WHERE id = 2"), vec![2]);
}

// ---------------------------------------------------------------------------
// Reading the WAL and the keyspace
// ---------------------------------------------------------------------------

/// How a key was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Put,
    Delete,
    SingleDelete,
    /// The start bound of a range deletion (the end bound is exclusive).
    RangeDelete,
    /// Present in the live keyspace after the last close.
    Live,
}

struct Observation {
    /// First sequence number of the WAL batch, `None` for the live keyspace.
    seq: Option<u64>,
    seen: Seen,
    key: Vec<u8>,
}

/// Puts and deletes as `WriteBatch::iterate` reports them, in batch order.
#[derive(Default)]
struct Recorder(Vec<(Seen, Vec<u8>)>);

impl WriteBatchIterator for Recorder {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.0.push((Seen::Put, key.into_vec()));
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push((Seen::Delete, key.into_vec()));
    }
}

/// A varint32-length-prefixed slice, as RocksDB's `GetLengthPrefixedSlice`.
fn take_slice<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let mut length = 0u32;
    let mut ended = false;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first().ok_or("truncated length")?;
        *input = rest;
        length |= u32::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            ended = true;
            break;
        }
    }
    let length = length as usize;
    if !ended || length > input.len() {
        return Err("truncated slice".into());
    }
    let (slice, rest) = input.split_at(length);
    *input = rest;
    Ok(slice)
}

/// Every sequence-consuming op of one raw default-column-family batch:
/// 12-byte header (sequence, count) then tagged records.
fn decode_raw_batch(raw: &[u8]) -> Result<Vec<(Seen, Vec<u8>)>, String> {
    let header = raw.get(..12).ok_or("batch shorter than its 12-byte header")?;
    let count = u32::from_le_bytes(header[8..12].try_into().map_err(|_| "header count")?) as usize;
    let mut input = &raw[12..];
    let mut ops = Vec::with_capacity(count);
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        match tag {
            // kTypeDeletion
            0 => ops.push((Seen::Delete, take_slice(&mut input)?.to_vec())),
            // kTypeValue
            1 => {
                let key = take_slice(&mut input)?.to_vec();
                take_slice(&mut input)?;
                ops.push((Seen::Put, key));
            }
            // kTypeLogData: consumes no sequence number.
            3 => {
                take_slice(&mut input)?;
            }
            // kTypeSingleDeletion
            7 => ops.push((Seen::SingleDelete, take_slice(&mut input)?.to_vec())),
            // kTypeRangeDeletion: [start, end)
            15 => {
                let start = take_slice(&mut input)?.to_vec();
                take_slice(&mut input)?;
                ops.push((Seen::RangeDelete, start));
            }
            other => {
                return Err(format!(
                    "unsupported RocksDB record type {other} (merge, column-family or 2PC record): \
                     WriteBatch::iterate would have dropped it unseen"
                ))
            }
        }
    }
    if ops.len() != count {
        return Err(format!("decoded {} ops but the header counts {count}", ops.len()));
    }
    Ok(ops)
}

/// Every key in the closed store's RocksDB WAL (from sequence 0) and live
/// keyspace, opened read-only.
fn observe(dir: &Path) -> Vec<Observation> {
    let mut opts = Options::default();
    opts.create_if_missing(false);
    let families = DB::list_cf(&opts, dir).expect("list column families");
    assert_eq!(
        families,
        vec!["default".to_string()],
        "a primary keeps everything in the default column family"
    );
    let db = DB::open_for_read_only(&opts, dir, false).expect("open the closed store read-only");
    let latest = db.latest_sequence_number();

    let mut observations = Vec::new();
    let mut next_seq = 1u64;
    let mut batches = 0usize;
    let mut wal = db
        .get_updates_since(0)
        .expect("get_updates_since(0): the RocksDB WAL must be retained from the first write");
    for item in wal.by_ref() {
        let (seq, batch) = item.expect("read a WAL batch");
        assert_eq!(
            seq, next_seq,
            "gap in the RocksDB WAL: expected sequence {next_seq}, found {seq}; keys in the gap \
             could not be classified"
        );
        let decoded = decode_raw_batch(batch.data()).unwrap_or_else(|e| panic!("WAL batch at sequence {seq}: {e}"));
        assert_eq!(decoded.len(), batch.len(), "WAL batch at sequence {seq}: op count");
        let mut recorder = Recorder::default();
        batch.iterate(&mut recorder);
        let decoded_puts_and_deletes: Vec<(Seen, Vec<u8>)> = decoded
            .iter()
            .filter(|(seen, _)| matches!(seen, Seen::Put | Seen::Delete))
            .cloned()
            .collect();
        assert!(
            recorder.0 == decoded_puts_and_deletes,
            "WAL batch at sequence {seq}: WriteBatch::iterate and the raw decoder disagree on the puts \
             and deletes"
        );
        observations.extend(decoded.into_iter().map(|(seen, key)| Observation {
            seq: Some(seq),
            seen,
            key,
        }));
        next_seq = seq + batch.len() as u64;
        batches += 1;
    }
    wal.status().expect("WAL iterator status");
    assert!(batches > 0, "the RocksDB WAL yielded no batches");
    assert_eq!(
        next_seq - 1,
        latest,
        "the WAL walk ended at sequence {} but the store's latest sequence is {latest}: part of \
         the write history was not retained",
        next_seq - 1
    );

    for item in db.iterator(IteratorMode::Start) {
        let (key, _) = item.expect("iterate the live keyspace");
        observations.push(Observation {
            seq: None,
            seen: Seen::Live,
            key: key.into_vec(),
        });
    }
    observations
}

fn escape(key: &[u8]) -> String {
    key.iter()
        .flat_map(|&b| std::ascii::escape_default(b))
        .map(char::from)
        .collect()
}

fn describe(o: &Observation) -> String {
    match o.seq {
        Some(seq) => format!("WAL seq {seq} {:?} {}", o.seen, escape(&o.key)),
        None => format!("live {}", escape(&o.key)),
    }
}

/// Every observation the classifier calls `Unknown`.
fn offenders(observations: &[Observation], class_of: impl Fn(&[u8]) -> KeyClass) -> Vec<String> {
    observations
        .iter()
        .filter(|o| class_of(&o.key) == KeyClass::Unknown)
        .map(describe)
        .collect()
}

/// Offenders grouped by namespace (the key up to its first ':'), then the
/// first few in full — a missing family can contribute thousands of keys.
fn report(offenders: &[String]) -> String {
    let mut by_namespace: BTreeMap<String, usize> = BTreeMap::new();
    for line in offenders {
        let key = line.rsplit(' ').next().unwrap_or_default();
        let namespace = key.split_once(':').map_or(key, |(ns, _)| ns);
        *by_namespace.entry(namespace.to_string()).or_default() += 1;
    }
    let mut out = String::new();
    for (namespace, count) in &by_namespace {
        out.push_str(&format!("  {namespace:?}: {count} key(s)\n"));
    }
    for line in offenders.iter().take(40) {
        out.push_str(&format!("    {line}\n"));
    }
    if offenders.len() > 40 {
        out.push_str(&format!("    … and {} more\n", offenders.len() - 40));
    }
    out
}

#[test]
fn a_primarys_wal_and_keyspace_hold_no_unknown_key() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("primary");

    let db = open(&dir);
    session_one(&db);
    drop(db);
    let db = open(&dir);
    session_two(&db);
    drop(db);

    let observations = observe(&dir);

    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen_classes: HashSet<KeyClass> = HashSet::new();
    for o in &observations {
        let class = classify(&o.key).class;
        seen_classes.insert(class);
        *histogram.entry(format!("{class:?}")).or_default() += 1;
    }
    println!("{} observations by class: {histogram:#?}", observations.len());

    // 1. The tripwire itself.
    let unknown = offenders(&observations, |key| classify(key).class);
    assert!(
        unknown.is_empty(),
        "{} key(s) the primary wrote classify as Unknown. Add the family to \
         src/storage/keyspace.rs (citing its constructor):\n{}",
        unknown.len(),
        report(&unknown)
    );

    // 2. Coverage: the corpus wrote what it claims to.
    let mut required: Vec<KeyClass> = REQUIRED_CLASSES.to_vec();
    if cfg!(feature = "vector-persist") {
        required.push(KeyClass::Vidx);
    }
    let missing: Vec<KeyClass> = required.into_iter().filter(|c| !seen_classes.contains(c)).collect();
    assert!(
        missing.is_empty(),
        "the corpus produced no key of {missing:?}; a statement stopped writing (or the WAL walk \
         missed it). Observed: {histogram:#?}"
    );

    // 3. Red proof: with `snapshot:` removed, exactly the snapshot keys are reported.
    let without_snapshot = |key: &[u8]| {
        if key.starts_with(b"snapshot:") {
            KeyClass::Unknown
        } else {
            classify(key).class
        }
    };
    let reported = offenders(&observations, without_snapshot);
    let only_snapshot_keys = reported
        .iter()
        .all(|line| line.rsplit(' ').next().is_some_and(|key| key.starts_with("snapshot:")));
    assert!(
        !reported.is_empty() && only_snapshot_keys,
        "dropping `snapshot:` must report snapshot keys and only those:\n{}",
        report(&reported)
    );
}
