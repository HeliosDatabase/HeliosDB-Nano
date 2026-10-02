//! External integration draft. Requires the strict storage/catalog APIs listed
//! in strict-hydration-draft/README.md; it does not test online publication.
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::path::{Path, PathBuf};

fn config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config
}

fn checkpoint() -> (tempfile::TempDir, PathBuf, Vec<Value>) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let snapshot = root.path().join("checkpoint");
    let db = EmbeddedDatabase::with_config(config(&source)).unwrap();
    db.execute("CREATE TABLE hydration_rows (id INT PRIMARY KEY, tag TEXT)")
        .unwrap();
    db.execute("INSERT INTO hydration_rows VALUES (1, 'before-checkpoint'), (2, 'second')")
        .unwrap();
    db.execute("CREATE FUNCTION hydration_double(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE FUNCTION hydration_rewrite() RETURNS TRIGGER AS $$ BEGIN NEW.tag = 'trigger-value'; RETURN NEW; END $$ LANGUAGE plpgsql")
        .unwrap();
    db.execute("CREATE TRIGGER hydration_trigger BEFORE INSERT ON hydration_rows FOR EACH ROW EXECUTE FUNCTION hydration_rewrite()")
        .unwrap();
    db.execute("CREATE PROCEDURE hydration_proc() LANGUAGE sql AS $$INSERT INTO hydration_rows VALUES (99, 'procedure-value')$$")
        .unwrap();
    let source_udf = db.query("SELECT hydration_double(7)", &[]).unwrap()[0].values.clone();
    let raw = db.storage.db();
    rocksdb::checkpoint::Checkpoint::new(&raw)
        .unwrap()
        .create_checkpoint(&snapshot)
        .unwrap();
    drop(raw);
    drop(db);
    (root, snapshot, source_udf)
}

fn raw_records(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), path, false).unwrap();
    db.iterator(rocksdb::IteratorMode::Start)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn raw_put(path: &Path, key: &[u8], value: &[u8]) {
    let db = rocksdb::DB::open(&rocksdb::Options::default(), path).unwrap();
    db.put(key, value).unwrap();
    db.flush().unwrap();
}

#[test]
fn sealed_checkpoint_hydrates_indexes_routines_and_triggers_without_durable_mutation() {
    let (_root, path, source_udf) = checkpoint();
    // Deliberately stale counter requires an in-memory correction, not a write
    // into this sealed checkpoint. Invalid logical WAL must never be replayed.
    raw_put(&path, b"counter:hydration_rows", &bincode::serialize(&0u64).unwrap());
    raw_put(&path, b"wal:entries:00000000000000000001", b"not-a-logical-wal-entry");
    let before = raw_records(&path);
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&path)).unwrap();
    assert_eq!(hydrated.report.indexes.vector_degraded, 0);
    assert_eq!(hydrated.report.functions, 2);
    assert_eq!(hydrated.report.triggers, 1);
    assert_eq!(hydrated.report.procedures, 1);
    assert!(!hydrated.database.storage.row_counter_flush_on_close());
    assert!(!hydrated.database.storage.index_snapshots_on_close());
    assert!(!hydrated.database.storage.wal_checkpoint_on_close());
    assert_eq!(
        hydrated
            .database
            .query("SELECT id, tag FROM hydration_rows ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|row| row.values)
            .collect::<Vec<_>>(),
        vec![
            vec![Value::Int4(1), Value::String("before-checkpoint".into())],
            vec![Value::Int4(2), Value::String("second".into())],
        ]
    );
    assert_eq!(
        hydrated.database.query("SELECT hydration_double(7)", &[]).unwrap()[0].values,
        source_udf
    );
    assert_eq!(
        hydrated.database.storage.art_indexes().pk_index_len("hydration_rows"),
        Some(2)
    );
    assert!(hydrated
        .database
        .trigger_registry
        .get_trigger("hydration_rows", "hydration_trigger")
        .unwrap()
        .is_some());
    drop(hydrated);
    assert_eq!(
        raw_records(&path),
        before,
        "hydration or Drop changed sealed logical records"
    );
}

#[test]
fn malformed_durable_runtime_objects_fail_with_keyed_diagnostics_and_preserve_bytes() {
    for (key, context) in [
        (b"meta:function:broken_function".as_slice(), "broken_function"),
        (b"meta:procedure:broken_procedure".as_slice(), "broken_procedure"),
        (b"trigger:hydration_rows:broken_trigger".as_slice(), "broken_trigger"),
        (
            b"trigger_rowmut:hydration_rows:broken_recipe".as_slice(),
            "broken_recipe",
        ),
        (b"meta:index:broken_index".as_slice(), "broken_index"),
        (b"__view_metadata__broken_view".as_slice(), "broken_view"),
        (b"meta:mv:broken_mv".as_slice(), "broken_mv"),
        (b"meta:sequence:broken_sequence".as_slice(), "broken_sequence"),
        (b"meta:seqstate:orphan_sequence".as_slice(), "orphan_sequence"),
        (b"branch:registry".as_slice(), "branch"),
        (b"snapshot:00000000000000000001".as_slice(), "snapshot"),
        (b"vmeta:broken_marker".as_slice(), "COPY"),
    ] {
        let (_root, path, _) = checkpoint();
        raw_put(&path, key, b"invalid persisted object");
        let before = raw_records(&path);
        let error = match EmbeddedDatabase::open_physical_snapshot(config(&path)) {
            Ok(_) => panic!("strict hydration accepted malformed object {context}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(context), "missing object context {context}: {error}");
        assert_eq!(raw_records(&path), before, "failure cleanup changed {context}");
    }
}

#[test]
fn physical_hydration_requires_a_persistent_existing_checkpoint() {
    assert!(EmbeddedDatabase::open_physical_snapshot(Config::in_memory()).is_err());
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("must-not-create");
    assert!(EmbeddedDatabase::open_physical_snapshot(config(&missing)).is_err());
    assert!(!missing.exists(), "strict open created a missing checkpoint");
}

#[test]
fn ordinary_open_keeps_its_existing_resilient_routine_behavior() {
    let (_root, path, source_udf) = checkpoint();
    raw_put(&path, b"meta:function:broken_function", b"invalid function");
    let ordinary = EmbeddedDatabase::with_config(config(&path)).unwrap();
    assert_eq!(
        ordinary.query("SELECT hydration_double(7)", &[]).unwrap()[0].values,
        source_udf
    );
}

#[test]
fn duplicate_normalized_routine_names_are_not_silently_overwritten() {
    for (prefix, name) in [
        ("meta:function:", "hydration_double"),
        ("meta:procedure:", "hydration_proc"),
    ] {
        let (_root, path, _) = checkpoint();
        let original_key = format!("{prefix}{name}").into_bytes();
        let bytes = raw_records(&path)
            .into_iter()
            .find(|(key, _)| key == &original_key)
            .unwrap()
            .1;
        let duplicate_key = format!("{prefix}{}", name.to_uppercase());
        raw_put(&path, duplicate_key.as_bytes(), &bytes);
        let before = raw_records(&path);
        let error = match EmbeddedDatabase::open_physical_snapshot(config(&path)) {
            Ok(_) => panic!("accepted duplicate normalized routine: {prefix}{name}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("duplicates a normalized routine name"), "{error}");
        assert_eq!(raw_records(&path), before);
    }
}

#[test]
fn present_malformed_sequence_state_is_not_treated_as_a_new_sequence() {
    let (_root, path, _) = checkpoint();
    let definition = heliosdb_nano::storage::PersistedSequence::default_named("state_owner");
    raw_put(
        &path,
        b"meta:sequence:state_owner",
        &bincode::serialize(&definition).unwrap(),
    );
    raw_put(&path, b"meta:seqstate:state_owner", b"invalid high-water state");
    let before = raw_records(&path);
    let error = match EmbeddedDatabase::open_physical_snapshot(config(&path)) {
        Ok(_) => panic!("malformed sequence state accepted"),
        Err(e) => e.to_string(),
    };
    assert!(error.contains("state_owner"), "{error}");
    assert_eq!(raw_records(&path), before);
}

#[test]
fn sealed_views_materialized_views_and_sequences_match_the_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let snapshot = root.path().join("checkpoint");
    let db = EmbeddedDatabase::with_config(config(&source)).unwrap();
    db.execute("CREATE TABLE lazy_rows (id INT PRIMARY KEY, tag TEXT)")
        .unwrap();
    db.execute("INSERT INTO lazy_rows VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    db.execute("CREATE VIEW lazy_view AS SELECT id, tag FROM lazy_rows")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW lazy_mv AS SELECT id, tag FROM lazy_rows")
        .unwrap();
    db.execute("CREATE SEQUENCE lazy_seq START 10").unwrap();
    db.query("SELECT nextval('lazy_seq')", &[]).unwrap();
    let queries = [
        "SELECT * FROM lazy_view ORDER BY id",
        "SELECT * FROM lazy_mv ORDER BY id",
    ];
    let expected: Vec<Vec<Vec<Value>>> = queries
        .iter()
        .map(|q| db.query(q, &[]).unwrap().into_iter().map(|row| row.values).collect())
        .collect();
    let state = db.storage.catalog().get_sequence_state("lazy_seq").unwrap();
    assert!(state.is_some());
    let raw = db.storage.db();
    rocksdb::checkpoint::Checkpoint::new(&raw)
        .unwrap()
        .create_checkpoint(&snapshot)
        .unwrap();
    drop(raw);
    drop(db);
    let before = raw_records(&snapshot);
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&snapshot)).unwrap();
    for (query, expected) in queries.iter().zip(expected) {
        let actual: Vec<Vec<Value>> = hydrated
            .database
            .query(query, &[])
            .unwrap()
            .into_iter()
            .map(|row| row.values)
            .collect();
        assert_eq!(actual, expected, "{query}");
    }
    assert_eq!(
        hydrated
            .database
            .storage
            .catalog()
            .get_sequence_state("lazy_seq")
            .unwrap(),
        state
    );
    drop(hydrated);
    assert_eq!(raw_records(&snapshot), before);

    // A valid outer MV record cannot hide garbage after its encoded plan.
    let raw = rocksdb::DB::open(&rocksdb::Options::default(), &snapshot).unwrap();
    let bytes = raw.get(b"meta:mv:lazy_mv").unwrap().unwrap();
    drop(raw);
    let mut metadata: heliosdb_nano::storage::MaterializedViewMetadata =
        bincode::deserialize(&bytes).unwrap();
    metadata.query_plan_bytes.push(0);
    raw_put(&snapshot, b"meta:mv:lazy_mv", &bincode::serialize(&metadata).unwrap());
    let before = raw_records(&snapshot);
    let error = match EmbeddedDatabase::open_physical_snapshot(config(&snapshot)) {
        Ok(_) => panic!("trailing MV plan bytes accepted"),
        Err(e) => e.to_string(),
    };
    assert!(error.contains("lazy_mv") && error.contains("plan"), "{error}");
    assert_eq!(raw_records(&snapshot), before);
}

#[test]
fn strict_view_decode_preserves_current_binding_and_exact_legacy_layout() {
    use heliosdb_nano::storage::{ViewCatalog, ViewMetadata};
    #[derive(serde::Serialize)]
    struct Legacy<'a> {
        view_name: &'a str,
        query_sql: &'a str,
        schema: &'a heliosdb_nano::Schema,
        created_at: chrono::DateTime<chrono::Utc>,
        or_replace: bool,
    }
    let metadata = ViewMetadata::new(
        "checked_view".into(),
        "SELECT 1".into(),
        heliosdb_nano::Schema::new(vec![]),
    );
    let legacy = bincode::serialize(&Legacy {
        view_name: &metadata.view_name,
        query_sql: &metadata.query_sql,
        schema: &metadata.schema,
        created_at: metadata.created_at,
        or_replace: false,
    })
    .unwrap();
    let current = metadata.clone().with_creator_schema(Some("bound_schema".into()));
    for (bytes, expected_schema) in [
        (legacy, None),
        (bincode::serialize(&current).unwrap(), Some("bound_schema")),
    ] {
        let (_root, path, _) = checkpoint();
        raw_put(&path, b"__view_metadata__checked_view", &bytes);
        let before = raw_records(&path);
        let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&path)).unwrap();
        let actual = ViewCatalog::new(&hydrated.database.storage)
            .get_view("checked_view")
            .unwrap();
        assert_eq!(actual.creator_schema.as_deref(), expected_schema);
        drop(hydrated);
        assert_eq!(raw_records(&path), before);
    }
    let mut invalid_option = bincode::serialize(&metadata).unwrap();
    assert_eq!(invalid_option.pop(), Some(0));
    invalid_option.push(2);
    let mut trailing = bincode::serialize(&current).unwrap();
    trailing.push(99);
    for bytes in [invalid_option, trailing] {
        let (_root, path, _) = checkpoint();
        raw_put(&path, b"__view_metadata__checked_view", &bytes);
        let before = raw_records(&path);
        let error = match EmbeddedDatabase::open_physical_snapshot(config(&path)) {
            Ok(_) => panic!("corrupt current view silently became a legacy view"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("checked_view"), "{error}");
        assert_eq!(raw_records(&path), before);
    }
}
