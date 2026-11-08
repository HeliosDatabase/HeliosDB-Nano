//! Routine registry and durable metadata must agree after every native outcome.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;

use heliosdb_nano::{Config, EmbeddedDatabase, Result, Value};
use rocksdb::{IteratorMode, WriteBatchIterator};

#[derive(Clone, Copy, Debug)]
enum Kind {
    Function,
    Procedure,
}
#[derive(Clone, Copy, Debug)]
enum Change {
    Create,
    Replace,
    Drop,
}
#[derive(Clone, Copy, Debug)]
enum Fault {
    Cap,
    Before,
    After,
    Applied,
}
#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn open(path: &Path) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn exec(db: &EmbeddedDatabase, family: Family, sql: &str) -> Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn create_sql(kind: Kind, name: &str, replace: bool, value: i32) -> String {
    let replace = if replace { "OR REPLACE " } else { "" };
    match kind {
        Kind::Function => {
            format!("CREATE {replace}FUNCTION {name}() RETURNS INTEGER AS $$ SELECT {value} $$ LANGUAGE sql")
        }
        Kind::Procedure => {
            format!("CREATE {replace}PROCEDURE {name}() LANGUAGE sql AS $$ INSERT INTO sink VALUES ({value}) $$")
        }
    }
}

fn prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::Function => "meta:function:",
        Kind::Procedure => "meta:procedure:",
    }
}

fn noun(kind: Kind) -> &'static str {
    match kind {
        Kind::Function => "FUNCTION",
        Kind::Procedure => "PROCEDURE",
    }
}

fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn records(db: &EmbeddedDatabase, kind: Kind, name: &str) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let prefix = prefix(kind);
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .filter_map(|item| {
            let (key, _) = item.unwrap();
            let suffix = std::str::from_utf8(&key).ok()?.strip_prefix(prefix)?;
            if !suffix.eq_ignore_ascii_case(name) {
                return None;
            }
            let key = key.to_vec();
            let value = db.storage.get(&key).unwrap().unwrap();
            Some((key, value))
        })
        .collect()
}

fn body_value(kind: Kind, body: &str) -> i32 {
    // The only bodies in this fixture are constant10 or20, independent of
    // formatting introduced by either planner/executor family.
    match (body.contains("10"), body.contains("20")) {
        (true, false) => 10,
        (false, true) => 20,
        other => panic!("unexpected {kind:?} body {body:?}: {other:?}"),
    }
}

fn registry_value(db: &EmbeddedDatabase, kind: Kind, name: &str) -> Option<i32> {
    match kind {
        Kind::Function => db
            .function_registry
            .get_function(name)
            .map(|routine| body_value(kind, &routine.body)),
        Kind::Procedure => db
            .function_registry
            .get_procedure(name)
            .map(|routine| body_value(kind, &routine.body)),
    }
}

fn durable_value(db: &EmbeddedDatabase, kind: Kind, name: &str) -> Option<i32> {
    let records = records(db, kind, name);
    assert!(
        records.len() <= 1,
        "one logical routine cannot retain competing case-spelled records"
    );
    records.into_values().next().map(|bytes| match kind {
        Kind::Function => body_value(
            kind,
            &bincode::deserialize::<heliosdb_nano::sql::StoredFunction>(&bytes)
                .unwrap()
                .body,
        ),
        Kind::Procedure => body_value(
            kind,
            &bincode::deserialize::<heliosdb_nano::sql::StoredProcedure>(&bytes)
                .unwrap()
                .body,
        ),
    })
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int4(value) => i64::from(*value),
        Value::Int8(value) => *value,
        other => panic!("expected numeric scalar, got {other:?}"),
    }
}

fn assert_callable(db: &EmbeddedDatabase, family: Family, kind: Kind, name: &str, expected: Option<i32>) {
    match kind {
        Kind::Function => {
            let sql = format!("SELECT {name}()");
            let result = match family {
                Family::Text => db.query(&sql, &[]),
                Family::Params => db.query_params(&sql, &[]),
            };
            match expected {
                Some(value) => {
                    let rows = result.unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(integer(rows[0].values.first().unwrap()), i64::from(value));
                }
                None => assert!(
                    result.is_err(),
                    "an absent routine must not serve a cached or eager body"
                ),
            }
        }
        Kind::Procedure => {
            db.execute("TRUNCATE TABLE sink").unwrap();
            let result = exec(db, family, &format!("CALL {name}()"));
            match expected {
                Some(value) => {
                    assert_eq!(result.unwrap(), 0);
                    let rows = db.query("SELECT observed FROM sink", &[]).unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(integer(rows[0].values.first().unwrap()), i64::from(value));
                }
                None => assert!(result.is_err()),
            }
        }
    }
}

struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

fn group_proof(db: &EmbeddedDatabase, kind: Kind, before: u64) {
    let raw = db.storage.db();
    raw.flush_wal(true).unwrap();
    let mut cursor = raw.get_updates_since(before).unwrap();
    let mut groups = 0;
    for item in cursor.by_ref() {
        let (sequence, batch) = item.unwrap();
        if sequence <= before || batch.is_empty() {
            continue;
        }
        assert_eq!(sequence, before + 1);
        assert_eq!(sequence + batch.len() as u64 - 1, raw.latest_sequence_number());
        let mut keys = Keys(Vec::new());
        batch.iterate(&mut keys);
        assert!(keys.0.iter().any(|key| key.starts_with(prefix(kind).as_bytes())));
        groups += 1;
    }
    cursor.status().unwrap();
    assert_eq!(groups, 1, "no error-path metadata repair group");
}

fn exercise(kind: Kind, change: Change, fault: Fault, family: Family) {
    let dir = tempfile::tempdir().unwrap();
    let before_value = (!matches!(change, Change::Create)).then_some(10);
    let desired = (!matches!(change, Change::Drop)).then_some(20);
    let applied = matches!(fault, Fault::After | Fault::Applied);
    let expected = if applied { desired } else { before_value };
    {
        let db = open(dir.path());
        db.execute("CREATE TABLE sink (observed INT)").unwrap();
        db.execute("CREATE TABLE healthy (id INT PRIMARY KEY)").unwrap();
        if before_value.is_some() {
            exec(&db, family, &create_sql(kind, "outcome_routine", false, 10)).unwrap();
        }
        assert_callable(&db, family, kind, "outcome_routine", before_value);
        assert_callable(&db, family, kind, "outcome_routine", before_value);
        let before_records = records(&db, kind, "outcome_routine");
        let bytes = image(&db);
        let before = db.storage.db().latest_sequence_number();
        match fault {
            Fault::Cap => db.storage.debug_set_write_group_ceiling(Some(0)),
            Fault::Before => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
            Fault::After => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
            Fault::Applied => heliosdb_nano::storage::ddl_batch::debug_fail_next_applied_commit(),
        }
        let sql = match change {
            Change::Create => create_sql(kind, "outcome_routine", false, 20),
            Change::Replace => create_sql(kind, "outcome_routine", true, 20),
            Change::Drop => format!("DROP {} outcome_routine", noun(kind)),
        };
        let result = exec(&db, family, &sql);
        db.storage.debug_set_write_group_ceiling(None);
        let error = result.expect_err("the original native/durability/refusal error must reach the SQL caller");
        let marker = match fault {
            Fault::Cap => heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER,
            Fault::Before => "injected native write error before apply; outcome is unknown",
            Fault::After => "injected native write error after apply; outcome is unknown",
            Fault::Applied => "injected schema durability failure after the group applied",
        };
        assert!(
            error.to_string().contains(marker),
            "{kind:?}/{change:?}/{fault:?}/{family:?}: {error}"
        );
        if applied {
            group_proof(&db, kind, before);
        } else {
            assert_eq!(db.storage.db().latest_sequence_number(), before);
            assert_eq!(
                image(&db),
                bytes,
                "a refused/unapplied routine group leaves all bytes unchanged"
            );
            assert_eq!(records(&db, kind, "outcome_routine"), before_records);
        }
        assert_eq!(durable_value(&db, kind, "outcome_routine"), expected);
        assert_eq!(
            registry_value(&db, kind, "outcome_routine"),
            expected,
            "live RAM must agree with proven durable truth"
        );
        assert_callable(&db, family, kind, "outcome_routine", expected);
        if !applied {
            exec(&db, family, &sql).unwrap();
            assert_eq!(durable_value(&db, kind, "outcome_routine"), desired);
            assert_eq!(registry_value(&db, kind, "outcome_routine"), desired);
            assert_callable(&db, family, kind, "outcome_routine", desired);
        }
        db.execute("INSERT INTO healthy VALUES (7)").unwrap();
        assert_eq!(
            db.query("SELECT id FROM healthy", &[]).unwrap()[0].values,
            vec![Value::Int4(7)]
        );
    }
    let db = open(dir.path());
    assert_eq!(durable_value(&db, kind, "outcome_routine"), desired);
    assert_eq!(registry_value(&db, kind, "outcome_routine"), desired);
    assert_callable(&db, family, kind, "outcome_routine", desired);
    eprintln!("ROUTINE_OUTCOME_CELL {kind:?} {change:?} {fault:?} {family:?}");
}

#[test]
fn function_create_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Create, Fault::Cap, family);
    }
}

#[test]
fn function_create_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Create, Fault::Before, family);
    }
}

#[test]
fn function_create_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Create, Fault::After, family);
    }
}

#[test]
fn function_create_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Create, Fault::Applied, family);
    }
}

#[test]
fn function_replace_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Replace, Fault::Cap, family);
    }
}

#[test]
fn function_replace_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Replace, Fault::Before, family);
    }
}

#[test]
fn function_replace_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Replace, Fault::After, family);
    }
}

#[test]
fn function_replace_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Replace, Fault::Applied, family);
    }
}

#[test]
fn function_drop_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Drop, Fault::Cap, family);
    }
}

#[test]
fn function_drop_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Drop, Fault::Before, family);
    }
}

#[test]
fn function_drop_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Drop, Fault::After, family);
    }
}

#[test]
fn function_drop_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Function, Change::Drop, Fault::Applied, family);
    }
}

#[test]
fn procedure_create_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Create, Fault::Cap, family);
    }
}

#[test]
fn procedure_create_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Create, Fault::Before, family);
    }
}

#[test]
fn procedure_create_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Create, Fault::After, family);
    }
}

#[test]
fn procedure_create_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Create, Fault::Applied, family);
    }
}

#[test]
fn procedure_replace_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Replace, Fault::Cap, family);
    }
}

#[test]
fn procedure_replace_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Replace, Fault::Before, family);
    }
}

#[test]
fn procedure_replace_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Replace, Fault::After, family);
    }
}

#[test]
fn procedure_replace_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Replace, Fault::Applied, family);
    }
}

#[test]
fn procedure_drop_cap_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Drop, Fault::Cap, family);
    }
}

#[test]
fn procedure_drop_before_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Drop, Fault::Before, family);
    }
}

#[test]
fn procedure_drop_after_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Drop, Fault::After, family);
    }
}

#[test]
fn procedure_drop_applied_native_outcome() {
    for family in FAMILIES {
        exercise(Kind::Procedure, Change::Drop, Fault::Applied, family);
    }
}

#[test]
fn replacing_case_spelled_routines_leaves_one_durable_identity_across_reopen() {
    for family in FAMILIES {
        for kind in [Kind::Function, Kind::Procedure] {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path());
                db.execute("CREATE TABLE sink (observed INT)").unwrap();
                exec(&db, family, &create_sql(kind, "mixed_name", false, 10)).unwrap();
                assert_callable(&db, family, kind, "mixed_name", Some(10));
                exec(&db, family, &create_sql(kind, "MiXeD_NaMe", true, 20)).unwrap();
                assert_eq!(
                    records(&db, kind, "mixed_name").len(),
                    1,
                    "same-group replacement must remove old case aliases"
                );
                assert_eq!(durable_value(&db, kind, "mixed_name"), Some(20));
                assert_eq!(registry_value(&db, kind, "mixed_name"), Some(20));
                assert_callable(&db, family, kind, "mixed_name", Some(20));
            }
            let db = open(dir.path());
            assert_eq!(records(&db, kind, "mixed_name").len(), 1);
            assert_eq!(registry_value(&db, kind, "mixed_name"), Some(20));
            assert_callable(&db, family, kind, "mixed_name", Some(20));
            exec(&db, family, &format!("DROP {} MIXED_NAME", noun(kind))).unwrap();
            assert!(records(&db, kind, "mixed_name").is_empty());
            assert_eq!(registry_value(&db, kind, "mixed_name"), None);
            assert_callable(&db, family, kind, "mixed_name", None);
            eprintln!("ROUTINE_ALIAS_CELL {kind:?} {family:?}");
        }
    }
}
