//! UUID equality must be an index probe, not a full scan — regression tests
//! for ISSUE-index-persistence-and-uuid-pointlookup.md §B.
//!
//! Every earlier UUID test asserted only row contents, which a full scan
//! satisfies identically; these assert the *access path* via the EXPLAIN
//! annotation (which mirrors the executor's own gates, so displayed plan ==
//! executed plan), plus result correctness against an unindexed twin.

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

const U1: &str = "11111111-2222-3333-4444-555555555555";
const U2: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

fn rows(rows: &[Tuple]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

fn build(db: &EmbeddedDatabase) {
    // UUID primary key table.
    db.execute("CREATE TABLE files (file_id UUID PRIMARY KEY, path TEXT)")
        .unwrap();
    // Secondary UUID index table + unindexed twin.
    for table in ["extr", "extr_twin"] {
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, file_id UUID, note TEXT)"
        ))
        .unwrap();
    }
    db.execute("CREATE INDEX extr_file_id ON extr(file_id)").unwrap();

    db.execute(&format!("INSERT INTO files VALUES ('{U1}', 'one.txt')"))
        .unwrap();
    db.execute(&format!("INSERT INTO files VALUES ('{U2}', 'two.txt')"))
        .unwrap();
    for table in ["extr", "extr_twin"] {
        db.execute(&format!("INSERT INTO {table} VALUES (1, '{U1}', 'n1')"))
            .unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (2, '{U2}', 'n2')"))
            .unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (3, '{U1}', 'n3')"))
            .unwrap();
    }
}

fn explain_text(db: &EmbeddedDatabase, sql: &str) -> String {
    let plan = db.query(sql, &[]).unwrap();
    plan.iter()
        .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn pk_literal_uuid_is_index_probe() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    let text = explain_text(&db, &format!("EXPLAIN SELECT path FROM files WHERE file_id = '{U1}'"));
    assert!(
        text.contains("Index Point Lookup using"),
        "PK UUID literal equality must probe the index, got:\n{text}"
    );
    let got = db
        .query(&format!("SELECT path FROM files WHERE file_id = '{U1}'"), &[])
        .unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn pk_cast_literal_uuid_is_index_probe() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    let text = explain_text(
        &db,
        &format!("EXPLAIN SELECT path FROM files WHERE file_id = '{U1}'::uuid"),
    );
    assert!(
        text.contains("Index Point Lookup using"),
        "PK UUID ::uuid-cast equality must probe the index, got:\n{text}"
    );
}

#[test]
fn secondary_literal_uuid_is_index_probe_and_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    let text = explain_text(&db, &format!("EXPLAIN SELECT id FROM extr WHERE file_id = '{U1}'"));
    assert!(
        text.contains("Index Point Lookup using extr_file_id"),
        "secondary UUID equality must probe extr_file_id, got:\n{text}"
    );
    let indexed = db
        .query(&format!("SELECT id FROM extr WHERE file_id = '{U1}'"), &[])
        .unwrap();
    let twin = db
        .query(&format!("SELECT id FROM extr_twin WHERE file_id = '{U1}'"), &[])
        .unwrap();
    assert_eq!(rows(&indexed), rows(&twin));
    assert_eq!(indexed.len(), 2);
}

#[test]
fn param_uuid_is_index_probe_and_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);

    // String-shaped parameter (text-format wire params arrive like this).
    let indexed = db
        .query_params("SELECT id FROM extr WHERE file_id = $1", &[Value::String(U1.into())])
        .unwrap();
    assert_eq!(indexed.len(), 2);

    // Uuid-shaped parameter (binary-format wire params with OID 2950).
    let uuid = uuid::Uuid::parse_str(U1).unwrap();
    let by_uuid = db
        .query_params("SELECT id FROM extr WHERE file_id = $1", &[Value::Uuid(uuid)])
        .unwrap();
    assert_eq!(rows(&indexed), rows(&by_uuid));
}

#[test]
fn raw_bytes_param_uuid_probes_and_matches() {
    // Binary-format parameter with no declared OID arrives as Value::Bytes —
    // 16 bytes are the UUID itself. Regression: this used to skip the probe
    // and full-scan.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    let uuid = uuid::Uuid::parse_str(U1).unwrap();
    let by_bytes = db
        .query_params(
            "SELECT id FROM extr WHERE file_id = $1",
            &[Value::Bytes(uuid.as_bytes().to_vec())],
        )
        .unwrap();
    let by_string = db
        .query_params("SELECT id FROM extr WHERE file_id = $1", &[Value::String(U1.into())])
        .unwrap();
    assert_eq!(rows(&by_bytes), rows(&by_string));
    assert_eq!(by_bytes.len(), 2);
}

#[test]
fn unindexed_uuid_column_is_not_advertised_as_probe() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    let text = explain_text(&db, &format!("EXPLAIN SELECT id FROM extr_twin WHERE file_id = '{U1}'"));
    assert!(
        !text.contains("Index Point Lookup"),
        "unindexed twin must not claim an index probe, got:\n{text}"
    );
}
