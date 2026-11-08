//! Rewrites must version logical values, not references to mutable side storage.
//! A past branch is created AFTER the parent changes its sidecars, so it cannot
//! accidentally pass by using the parent's still-current values at the anchor.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

fn execute(db: &EmbeddedDatabase, family: Family, sql: &str) {
    let result = match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    };
    result.unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"));
}

fn query(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Tuple> {
    match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    }
    .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"))
}

fn values(rows: Vec<Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

fn expected(payload: &str) -> Vec<Vec<Value>> {
    vec![vec![
        Value::Int4(1),
        Value::Int8(100),
        Value::String("one".to_string()),
        Value::String(payload.to_string()),
        Value::Int8(700),
        Value::String("plain".to_string()),
    ]]
}

const SELECT: &str = "SELECT id, c, d, x, p, r FROM s";

fn setup(db: &EmbeddedDatabase, family: Family, payload: &str) {
    execute(
        db,
        family,
        "CREATE TABLE s (id INT PRIMARY KEY, gone INT, c BIGINT STORAGE COLUMNAR, \
         d TEXT STORAGE DICTIONARY, x TEXT STORAGE CONTENT_ADDRESSED, p BIGINT, r TEXT)",
    );
    // Use the public storage API to ensure every family reads the same compact
    // physical row. SQL transactions intentionally retain logical row values and
    // would otherwise leave the side-reference bug unexercised on that family.
    db.storage
        .insert_tuple_versioned(
            "s",
            Tuple::new(vec![
                Value::Int4(1),
                Value::Int4(9),
                Value::Int8(100),
                Value::String("one".to_string()),
                Value::String(payload.to_string()),
                Value::Int8(700),
                Value::String("plain".to_string()),
            ]),
        )
        .unwrap();
    let raw = db.storage.db().get(b"data:s:1").unwrap().unwrap();
    let stored: Tuple = bincode::deserialize(&raw).unwrap();
    assert!(matches!(stored.values[2], Value::ColumnarRef));
    assert!(matches!(stored.values[3], Value::DictRef { .. }));
    assert!(matches!(stored.values[4], Value::CasRef { .. }));
    assert_eq!(values(query(db, family, SELECT)), expected(payload));
}

fn assert_no_storage_references(db: &EmbeddedDatabase, anchor: u64) {
    let timestamp = db.storage.snapshot_manager().resolve_scn(anchor).unwrap();
    let rows = db.storage.scan_table_at_snapshot("s", timestamp).unwrap();
    assert_eq!(rows.len(), 1, "the history check must inspect a row");
    for value in &rows[0].values {
        assert!(
            !matches!(value, Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef),
            "a historical row exposed a storage reference: {value:?}"
        );
    }
}

fn assert_anchor(db: &EmbeddedDatabase, family: Family, anchor: u64, payload: &str) {
    assert_anchor_rows(db, family, anchor, expected(payload));
}

fn assert_anchor_rows(db: &EmbeddedDatabase, family: Family, anchor: u64, expected: Vec<Vec<Value>>) {
    assert_no_storage_references(db, anchor);
    let sql = format!("{SELECT} AS OF SCN {anchor}");
    assert_eq!(values(query(db, family, &sql)), expected, "main AS OF");
    let aliased = format!(
        "SELECT id AS row_key, c AS old_columnar, d AS old_dictionary, \
        x AS old_cas, p AS old_p, r AS old_r FROM s AS OF SCN {anchor}"
    );
    assert_eq!(
        values(query(db, family, &aliased)),
        expected,
        "aliased historical projection"
    );
    db.execute(&format!("CREATE BRANCH past FROM main AS OF SCN {anchor}"))
        .unwrap();
    let branch = db
        .with_request_branch_scope("past", || Ok(query(db, family, SELECT)))
        .unwrap();
    assert_eq!(values(branch), expected, "past branch's logical row");
    db.execute("DROP BRANCH past").unwrap();
}

fn mutate_side_storage(db: &EmbeddedDatabase, family: Family) {
    let replacement = "new".repeat(800);
    execute(
        db,
        family,
        &format!("UPDATE s SET c = 200, d = 'two', x = '{replacement}', p = 900, r = 'later' WHERE id = 1"),
    );
    let rows = values(query(db, family, SELECT));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Int8(200), "the sidecar really changed");
    assert_eq!(rows[0][2], Value::String("two".to_string()));
    assert_eq!(rows[0][3], Value::String(replacement));
}

#[test]
fn rewrite_history_and_past_branches_keep_logical_side_storage_values() {
    let payload = "old".repeat(800);
    for family in [Family::Text, Family::Params] {
        for alter in [
            "ALTER TABLE s ADD COLUMN z UUID DEFAULT gen_random_uuid()",
            "ALTER TABLE s DROP COLUMN gone",
            "ALTER TABLE s DROP COLUMN gone, ADD COLUMN z UUID DEFAULT gen_random_uuid()",
            "ALTER TABLE s ALTER COLUMN r SET STORAGE DICTIONARY",
            "ALTER TABLE s ALTER COLUMN p SET STORAGE COLUMNAR",
        ] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            setup(&db, family, &payload);
            execute(&db, family, alter);
            let anchor = db.storage.snapshot_manager().current_scn().saturating_sub(1);
            assert_anchor(&db, family, anchor, &payload);
            mutate_side_storage(&db, family);
            // A resolver using today's columnar slot would return 200 here.
            assert_anchor(&db, family, anchor, &payload);
        }
    }
}

fn open(path: &Path) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    EmbeddedDatabase::with_config(config).unwrap()
}

#[test]
fn logical_rewrite_history_and_past_branches_survive_reopen() {
    let payload = "old".repeat(800);
    let temp = tempfile::tempdir().unwrap();
    let anchor = {
        let db = open(temp.path());
        setup(&db, Family::Text, &payload);
        execute(
            &db,
            Family::Text,
            "ALTER TABLE s DROP COLUMN gone, ADD COLUMN z UUID DEFAULT gen_random_uuid()",
        );
        let anchor = db.storage.snapshot_manager().current_scn().saturating_sub(1);
        mutate_side_storage(&db, Family::Text);
        assert_anchor(&db, Family::Text, anchor, &payload);
        anchor
    };
    let db = open(temp.path());
    assert_anchor(&db, Family::Text, anchor, &payload);
}

#[test]
fn rewrite_materializes_a_logical_preimage_for_a_row_created_without_history() {
    let payload = "old".repeat(800);
    let temp = tempfile::tempdir().unwrap();
    {
        let mut config = Config::default();
        config.storage.path = Some(temp.path().to_path_buf());
        config.storage.memory_only = false;
        config.storage.time_travel_enabled = false;
        let db = EmbeddedDatabase::with_config(config).unwrap();
        setup(&db, Family::Text, &payload);
    }
    let db = open(temp.path());
    db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO tick VALUES (1)").unwrap();
    let before = db.storage.snapshot_manager().current_scn().saturating_sub(1);
    execute(
        &db,
        Family::Text,
        "ALTER TABLE s ADD COLUMN z UUID DEFAULT gen_random_uuid()",
    );
    assert_anchor(&db, Family::Text, before, &payload);
    mutate_side_storage(&db, Family::Text);
    assert_anchor(&db, Family::Text, before, &payload);
}

fn capped_side_storage(children: bool) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.max_atomic_rewrite_bytes = 1024 * 1024;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute(
        "CREATE TABLE cap (id INT PRIMARY KEY, gone INT, d TEXT STORAGE DICTIONARY, \
         x TEXT STORAGE CONTENT_ADDRESSED, c BIGINT STORAGE COLUMNAR)",
    )
    .unwrap();
    let dictionary = "d".repeat(3500);
    let content = "x".repeat(3500);
    // This public storage API guarantees compact physical rows; a multi-row SQL
    // transaction deliberately stores logical rows and would not exercise the bug.
    for id in 1..=60 {
        db.storage
            .insert_tuple_versioned(
                "cap",
                Tuple::new(vec![
                    Value::Int4(id),
                    Value::Int4(9),
                    Value::String(dictionary.clone()),
                    Value::String(content.clone()),
                    Value::Int8(i64::from(id)),
                ]),
            )
            .unwrap();
    }
    let raw = db.storage.db().get(b"data:cap:1").unwrap().unwrap();
    let stored: Tuple = bincode::deserialize(&raw).unwrap();
    assert!(matches!(stored.values[2], Value::DictRef { .. }));
    assert!(matches!(stored.values[3], Value::CasRef { .. }));
    assert!(matches!(stored.values[4], Value::ColumnarRef));
    if children {
        db.execute("CREATE BRANCH child_a FROM main").unwrap();
        db.execute("CREATE BRANCH child_b FROM main").unwrap();
    }
    db
}

#[test]
fn rewrite_cap_counts_expanded_frozen_rows_and_refuses_without_changing_parent_or_children() {
    for family in [Family::Text, Family::Params] {
        for alter in [
            "ALTER TABLE cap DROP COLUMN gone",
            "ALTER TABLE cap ADD COLUMN z UUID DEFAULT gen_random_uuid()",
        ] {
            // The primary's own new logical versions fit the cap. It is the two
            // children's expanded copies that put the combined group above it.
            let control = capped_side_storage(false);
            execute(&control, family, alter);

            let db = capped_side_storage(true);
            let before = values(query(&db, family, "SELECT * FROM cap ORDER BY id"));
            assert_eq!(before.len(), 60);
            let error = match family {
                Family::Text => db.execute(alter),
                Family::Params => db.execute_params(alter, &[]),
            }
            .expect_err("expanded logical copies exceed the configured 1 MiB cap")
            .to_string();
            assert!(
                error.contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER),
                "{error}"
            );
            assert_eq!(values(query(&db, family, "SELECT * FROM cap ORDER BY id")), before);
            for child in ["child_a", "child_b"] {
                let rows = db
                    .with_request_branch_scope(child, || Ok(query(&db, family, "SELECT * FROM cap ORDER BY id")))
                    .unwrap();
                assert_eq!(values(rows), before, "{child} changed after refused {alter}");
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum LegacyHistory {
    Absent,
    CopyMarker,
    ElidedLatest,
}

/// Simulate the durable output of an older writer, using the public storage
/// handle. The payload was originally written without history, so no invented
/// logical v: image can accidentally make a materialization test pass.
fn legacy_physical_history(path: &Path, mode: LegacyHistory, payload: &str) -> u64 {
    {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config.storage.time_travel_enabled = false;
        config.storage.version_gc_interval_secs = Some(0);
        let db = EmbeddedDatabase::with_config(config).unwrap();
        setup(&db, Family::Text, payload);
        assert!(db.storage.db().get(b"v:s:1:0").unwrap().is_none());
    }
    let db = open(path);
    db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO tick VALUES (1)").unwrap();
    let anchor = db.storage.snapshot_manager().current_scn().saturating_sub(1);
    let timestamp = db.storage.snapshot_manager().resolve_scn(anchor).unwrap();
    match mode {
        LegacyHistory::Absent => {}
        LegacyHistory::CopyMarker => {
            let key = format!("vmeta:s:{:020}:{:020}", 1, 1);
            db.storage.db().put(key.as_bytes(), timestamp.to_be_bytes()).unwrap();
        }
        LegacyHistory::ElidedLatest => {
            let key = format!("v_idx:s:1:{:020}", u64::MAX - timestamp);
            db.storage
                .db()
                .put(key.as_bytes(), (timestamp | (1_u64 << 63)).to_be_bytes())
                .unwrap();
            db.storage.db().put(b"w3_2_elide_used", [1_u8]).unwrap();
        }
    }
    anchor
}

#[test]
fn transaction_materializes_legacy_side_storage_preimages_before_update_and_delete() {
    let payload = "old".repeat(800);
    for family in [Family::Text, Family::Params] {
        for mode in [
            LegacyHistory::Absent,
            LegacyHistory::CopyMarker,
            LegacyHistory::ElidedLatest,
        ] {
            for delete in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let before = legacy_physical_history(temp.path(), mode, &payload);
                {
                    let db = open(temp.path());
                    execute(&db, family, "BEGIN");
                    if delete {
                        execute(&db, family, "DELETE FROM s WHERE id = 1");
                    } else {
                        mutate_side_storage(&db, family);
                    }
                    execute(&db, family, "COMMIT");
                    assert_anchor(&db, family, before, &payload);
                    if delete {
                        assert!(query(&db, family, SELECT).is_empty());
                    }
                }
                let db = open(temp.path());
                assert_anchor(&db, family, before, &payload);
                if delete {
                    assert!(query(&db, family, SELECT).is_empty());
                }
            }
        }
    }
}

#[test]
fn transaction_physical_new_version_uses_pending_columnar_values_and_cannot_be_elided() {
    use heliosdb_nano::storage::columnar::ColumnarStore;
    use heliosdb_nano::storage::content_addr::ContentAddressedStore;
    use heliosdb_nano::storage::dictionary::ColumnDictionary;

    let payload = "old".repeat(800);
    let temp = tempfile::tempdir().unwrap();
    let before = legacy_physical_history(temp.path(), LegacyHistory::CopyMarker, &payload);
    let pending_payload = "pending".repeat(400);
    let after = {
        let mut config = Config::default();
        config.storage.path = Some(temp.path().to_path_buf());
        config.storage.memory_only = false;
        config.storage.elide_latest_version = true;
        config.storage.version_gc_interval_secs = Some(0);
        let db = EmbeddedDatabase::with_config(config).unwrap();
        let raw = db.storage.db().get(b"data:s:1").unwrap().unwrap();
        let mut physical: Tuple = bincode::deserialize(&raw).unwrap();
        let txn = db.storage.begin_transaction().unwrap();
        ColumnarStore::store_in_transaction(&db.storage.db(), &txn, "s", "c", 1, Value::Int8(222)).unwrap();
        let mut dictionary: ColumnDictionary =
            bincode::deserialize(&db.storage.db().get(b"dict:s:d").unwrap().unwrap()).unwrap();
        let dict_id = dictionary.encode("pending-dictionary");
        txn.put(b"dict:s:d".to_vec(), bincode::serialize(&dictionary).unwrap())
            .unwrap();
        physical.values[3] = Value::DictRef { dict_id };
        let hash = ContentAddressedStore::compute_hash(&Value::String(pending_payload.clone())).unwrap();
        let mut cas_key = b"cas:".to_vec();
        cas_key.extend_from_slice(&hash);
        txn.put(cas_key, pending_payload.as_bytes().to_vec()).unwrap();
        physical.values[4] = Value::CasRef { hash };
        txn.put(b"data:s:1".to_vec(), bincode::serialize(&physical).unwrap())
            .unwrap();
        txn.commit().unwrap();
        let after = db.storage.snapshot_manager().current_scn().saturating_sub(1);
        let timestamp = db.storage.snapshot_manager().resolve_scn(after).unwrap();
        let version = db.storage.db().get(format!("v:s:1:{timestamp}")).unwrap();
        assert!(version.is_some(), "a physical data row must retain a full logical v:");
        let mut wanted = expected(&payload);
        wanted[0][1] = Value::Int8(222);
        wanted[0][2] = Value::String("pending-dictionary".to_string());
        wanted[0][3] = Value::String(pending_payload.clone());
        assert_anchor_rows(&db, Family::Text, after, wanted);
        // A decoder that reads the old DB slot while building the commit would
        // preserve 100 as the NEW value; one that resolves history later would
        // return this subsequent 200. Both are wrong: the version must be 222.
        mutate_side_storage(&db, Family::Text);
        assert_anchor(&db, Family::Text, before, &payload);
        after
    };
    let db = open(temp.path());
    assert_anchor(&db, Family::Text, before, &payload);
    let mut wanted = expected(&payload);
    wanted[0][1] = Value::Int8(222);
    wanted[0][2] = Value::String("pending-dictionary".to_string());
    wanted[0][3] = Value::String(pending_payload);
    assert_anchor_rows(&db, Family::Text, after, wanted);
}

fn projected_storage_tuple(tuple: Tuple, has_gone: bool) -> Vec<Value> {
    let positions: &[usize] = if has_gone {
        &[0, 2, 3, 4, 5, 6]
    } else {
        &[0, 1, 2, 3, 4, 5]
    };
    positions
        .iter()
        .map(|position| tuple.values[*position].clone())
        .collect()
}

fn existing_row_anchor(db: &EmbeddedDatabase, table: &str, row: u64) -> (u64, u64) {
    let prefix = format!("v_idx:{table}:{row}:");
    let (key, value) = db
        .storage
        .db()
        .prefix_iterator(prefix.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    assert!(
        key.starts_with(prefix.as_bytes()),
        "the fixture needs a real version event"
    );
    let timestamp = u64::from_be_bytes(value.as_ref().try_into().unwrap()) & !(1_u64 << 63);
    let metadata = db.storage.snapshot_manager().get_snapshot_metadata(timestamp).unwrap();
    (timestamp, metadata.scn)
}

fn assert_versions(db: &EmbeddedDatabase, family: Family, before: u64, after: u64, wanted: &[Vec<Value>]) {
    let rows = values(query(
        db,
        family,
        &format!("{SELECT} VERSIONS BETWEEN SCN {before} AND SCN {after}"),
    ));
    assert!(!rows.is_empty(), "VERSIONS must inspect retained images");
    for row in rows {
        assert!(wanted.contains(&row), "unexpected historical version: {row:?}");
    }
}

#[test]
fn logical_history_crosses_storage_migrations_in_all_historical_read_funnels() {
    let payload = "old".repeat(800);
    for family in [Family::Text, Family::Params] {
        let temp = tempfile::tempdir().unwrap();
        let (before, after) = {
            let db = open(temp.path());
            setup(&db, family, &payload);
            let (_, before) = existing_row_anchor(&db, "s", 1);
            let reader = db.storage.begin_transaction().unwrap();
            for column in ["c", "d", "x"] {
                execute(
                    &db,
                    family,
                    &format!("ALTER TABLE s ALTER COLUMN {column} SET STORAGE DEFAULT"),
                );
                assert_anchor(&db, family, before, &payload);
            }
            let after = db.storage.snapshot_manager().current_scn().saturating_sub(1);
            let old: Tuple = bincode::deserialize(&reader.get(&b"data:s:1".to_vec()).unwrap().unwrap()).unwrap();
            assert_eq!(projected_storage_tuple(old, true), expected(&payload)[0]);
            reader.rollback().unwrap();
            assert_versions(&db, family, before, after, &expected(&payload));
            // Re-enter all representations, so an old anchor is read while the
            // current row is physical again, then overwrite every side value.
            for (column, mode) in [("c", "COLUMNAR"), ("d", "DICTIONARY"), ("x", "CONTENT_ADDRESSED")] {
                execute(
                    &db,
                    family,
                    &format!("ALTER TABLE s ALTER COLUMN {column} SET STORAGE {mode}"),
                );
            }
            mutate_side_storage(&db, family);
            assert_anchor(&db, family, before, &payload);
            assert_anchor(&db, family, after, &payload);
            (before, after)
        };
        let db = open(temp.path());
        assert_anchor(&db, family, before, &payload);
        assert_anchor(&db, family, after, &payload);
        assert_versions(&db, family, before, after, &expected(&payload));
    }
}

#[test]
fn legacy_immutable_references_are_decoded_after_layout_mapping_and_missing_payloads_are_typed() {
    let payload = "old".repeat(800);
    let temp = tempfile::tempdir().unwrap();
    let (before, after, hash) = {
        let db = open(temp.path());
        setup(&db, Family::Text, &payload);
        let (timestamp, before) = existing_row_anchor(&db, "s", 1);
        let mut legacy: Tuple = bincode::deserialize(&db.storage.db().get(b"data:s:1").unwrap().unwrap()).unwrap();
        legacy.values[2] = Value::Int8(100); // retain only immutable references
        let hash = match legacy.values[4] {
            Value::CasRef { hash } => hash,
            _ => panic!("CAS fixture"),
        };
        db.storage
            .db()
            .put(format!("v:s:1:{timestamp}"), bincode::serialize(&legacy).unwrap())
            .unwrap();
        db.storage.snapshot_manager().clear_cache();
        let reader = db.storage.begin_transaction().unwrap();
        db.execute("ALTER TABLE s DROP COLUMN gone").unwrap();
        db.execute("ALTER TABLE s ALTER COLUMN d SET STORAGE DEFAULT").unwrap();
        db.execute("ALTER TABLE s ALTER COLUMN x SET STORAGE DEFAULT").unwrap();
        let after = db.storage.snapshot_manager().current_scn().saturating_sub(1);
        assert_anchor(&db, Family::Text, before, &payload);
        let old: Tuple = bincode::deserialize(&reader.get(&b"data:s:1".to_vec()).unwrap().unwrap()).unwrap();
        assert_eq!(projected_storage_tuple(old, false), expected(&payload)[0]);
        reader.rollback().unwrap();
        assert_versions(&db, Family::Text, before, after, &expected(&payload));
        mutate_side_storage(&db, Family::Text);
        assert_anchor(&db, Family::Text, before, &payload);
        (before, after, hash)
    };
    let db = open(temp.path());
    assert_anchor(&db, Family::Text, before, &payload);
    // An immutable payload can be read even after SET STORAGE; an absent one
    // cannot. The new logical migration version remains readable in both cases.
    let mut key = b"cas:".to_vec();
    key.extend_from_slice(&hash);
    db.storage.db().delete(key).unwrap();
    drop(db);
    let db = open(temp.path());
    let error = db
        .query(&format!("{SELECT} AS OF SCN {before}"), &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(heliosdb_nano::BRANCH_ANCHOR_UNREADABLE_MARKER),
        "{error}"
    );
    assert!(error.contains("CAS payload is missing"), "{error}");
    assert_anchor(&db, Family::Text, after, &payload);
}

#[test]
fn legacy_columnar_version_refuses_instead_of_using_todays_slot_and_new_logical_versions_work() {
    let payload = "old".repeat(800);
    let temp = tempfile::tempdir().unwrap();
    let (before, after) = {
        let db = open(temp.path());
        setup(&db, Family::Text, &payload);
        let (timestamp, before) = existing_row_anchor(&db, "s", 1);
        let raw = db.storage.db().get(b"data:s:1").unwrap().unwrap();
        db.storage.db().put(format!("v:s:1:{timestamp}"), raw).unwrap();
        db.storage.snapshot_manager().clear_cache();
        let reader = db.storage.begin_transaction().unwrap();
        db.execute("ALTER TABLE s ALTER COLUMN c SET STORAGE DEFAULT").unwrap();
        let after = db.storage.snapshot_manager().current_scn().saturating_sub(1);
        let check_error = |error: String| {
            assert!(
                error.contains(heliosdb_nano::BRANCH_ANCHOR_UNREADABLE_MARKER),
                "{error}"
            );
            assert!(error.contains("legacy ColumnarRef"), "{error}");
        };
        check_error(
            db.query(&format!("{SELECT} AS OF SCN {before}"), &[])
                .unwrap_err()
                .to_string(),
        );
        check_error(reader.get(&b"data:s:1".to_vec()).unwrap_err().to_string());
        reader.rollback().unwrap();
        check_error(
            db.query(&format!("{SELECT} VERSIONS BETWEEN SCN {before} AND SCN {after}"), &[])
                .unwrap_err()
                .to_string(),
        );
        check_error(
            db.execute(&format!("CREATE BRANCH bad FROM main AS OF SCN {before}"))
                .unwrap_err()
                .to_string(),
        );
        assert!(db.storage.get_branch("bad").is_err());
        assert_anchor(&db, Family::Text, after, &payload);
        mutate_side_storage(&db, Family::Text);
        assert_anchor(&db, Family::Text, after, &payload);
        (before, after)
    };
    let db = open(temp.path());
    let error = db
        .query(&format!("{SELECT} AS OF SCN {before}"), &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("legacy ColumnarRef"), "{error}");
    assert_anchor(&db, Family::Text, after, &payload);
}

#[test]
fn dropping_an_unrecoverable_legacy_field_preserves_the_readable_columns() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE dead (id INT PRIMARY KEY, gone BIGINT STORAGE COLUMNAR, kept TEXT STORAGE DICTIONARY)")
        .unwrap();
    db.storage
        .insert_tuple_versioned(
            "dead",
            Tuple::new(vec![Value::Int4(1), Value::Int8(99), Value::String("kept".to_string())]),
        )
        .unwrap();
    let (timestamp, before) = existing_row_anchor(&db, "dead", 1);
    let raw = db.storage.db().get(b"data:dead:1").unwrap().unwrap();
    db.storage.db().put(format!("v:dead:1:{timestamp}"), raw).unwrap();
    db.storage.snapshot_manager().clear_cache();
    db.execute("ALTER TABLE dead DROP COLUMN gone").unwrap();
    assert_eq!(
        values(
            db.query(&format!("SELECT * FROM dead AS OF SCN {before}"), &[])
                .unwrap()
        ),
        vec![vec![Value::Int4(1), Value::String("kept".to_string())]]
    );
    db.execute(&format!("CREATE BRANCH intact FROM main AS OF SCN {before}"))
        .unwrap();
    let branch = db
        .with_request_branch_scope("intact", || db.query("SELECT * FROM dead", &[]))
        .unwrap();
    assert_eq!(
        values(branch),
        vec![vec![Value::Int4(1), Value::String("kept".to_string())]]
    );
}
