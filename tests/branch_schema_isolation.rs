//! sprinter 6bc4a4040eb7, phase B3 — schema DDL on a branch.
//!
//! Plan: `docs/plans/BRANCH_ISOLATION.md` (D4). Spec:
//! `docs/plans/specs/B3_branch_schema.md`.
//!
//! On a branch, `CREATE TABLE`, `DROP TABLE` and `ALTER TABLE … ADD COLUMN`
//! change the branch's own catalog overlay (`storage::branch_catalog`); every
//! other schema statement is refused (0A000) before it changes anything;
//! `MERGE BRANCH` carries the branch's schema changes into main, or refuses
//! (55000) when main changed an affected table after the branch did.
//!
//! Every scenario runs on BOTH DML executor families — the text family
//! (`execute` / `query` with literals) and the params family (`execute_params`
//! / `query_params`, the extended protocol's funnel) — and checks MAIN from the
//! raw keyspace (its `data:` rows and its catalog records), never only through
//! SQL, so no scenario can pass while writing main.
//!
//! On the pre-B3 tree each scenario fails: the branch's `ADD COLUMN` rewrote
//! main's catalog and rows (main grew the column), `CREATE TABLE` on a branch
//! created a MAIN table (visible on main and on every sibling), `DROP TABLE` on
//! a branch dropped main's table and purged main's rows, the refused
//! statements ran against main's catalog (DROP COLUMN, RENAME, CREATE VIEW,
//! CREATE SEQUENCE, CREATE FUNCTION) or failed with an unrelated error, and
//! `MERGE BRANCH` never refused anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::storage::branch_catalog::MergeFailSite;
use heliosdb_nano::storage::BranchState;
use heliosdb_nano::{
    EmbeddedDatabase, Error, Tuple, Value, BRANCH_MERGE_BUSY_MARKER, BRANCH_MERGE_DUPLICATE_KEY_MARKER,
    BRANCH_MERGE_KEY_CLAIMED_MARKER, BRANCH_MERGE_PENDING_MARKER, BRANCH_MERGE_SCHEMA_REFUSED_MARKER,
    BRANCH_SCHEMA_UNSUPPORTED_MARKER, BRANCH_TABLE_BUSY_MARKER, BRANCH_WRITE_UNSUPPORTED_MARKER,
    TABLE_LOCK_TIMEOUT_MARKER,
};
use tokio_postgres::{Client, NoTls};

// ---------------------------------------------------------------------------
// The two executor families
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `execute` / `query` with the parameters inlined as SQL literals.
    Text,
    /// `execute_params` / `query_params` with bound `$n`.
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => panic!("no SQL literal for {other:?}"),
    }
}

/// `sql` with each `$n` replaced by the literal of `params[n - 1]`.
fn inline(sql: &str, params: &[Value]) -> String {
    let mut out = sql.to_string();
    for (index, value) in params.iter().enumerate().rev() {
        out = out.replace(&format!("${}", index + 1), &literal(value));
    }
    out
}

fn exec(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(&inline(sql, params)),
        Family::Params => db.execute_params(sql, params),
    }
}

fn ok(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> u64 {
    exec(db, family, sql, params).unwrap_or_else(|e| panic!("[{family:?}] {sql} {params:?}: {e}"))
}

fn try_rows(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> heliosdb_nano::Result<Vec<Tuple>> {
    match family {
        Family::Text => db.query(&inline(sql, params), &[]),
        Family::Params => db.query_params(sql, params),
    }
}

fn rows(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> Vec<Tuple> {
    try_rows(db, family, sql, params).unwrap_or_else(|e| panic!("[{family:?}] {sql} {params:?}: {e}"))
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        other => panic!("expected text, got {other:?}"),
    }
}

fn ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter().map(|row| int(row.values.first())).collect()
}

fn i(v: i32) -> Value {
    Value::Int4(v)
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn branch(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!("CREATE BRANCH {name} AS OF NOW")).unwrap();
    db.execute(&format!("USE BRANCH {name}")).unwrap();
}

/// A schema statement refused on a branch (0A000) with the B3 refusal.
fn assert_refused_on_branch(result: heliosdb_nano::Result<u64>, family: Family, what: &str) {
    match result {
        Err(err) if err.to_string().contains(BRANCH_SCHEMA_UNSUPPORTED_MARKER) => {}
        Err(err) => panic!("[{family:?}] {what}: expected the 0A000 branch refusal, got: {err}"),
        Ok(n) => panic!("[{family:?}] {what}: expected the 0A000 branch refusal, the statement succeeded ({n})"),
    }
}

fn is_unique_violation(err: &Error) -> bool {
    match err {
        Error::ConstraintViolation(message) => {
            let lower = message.to_ascii_lowercase();
            !lower.contains("foreign key") && (lower.contains("duplicate key") || lower.contains("unique constraint"))
        }
        _ => false,
    }
}

fn assert_unique_violation(result: heliosdb_nano::Result<u64>, family: Family, what: &str) {
    match result {
        Err(err) if is_unique_violation(&err) => {}
        Err(err) => panic!("[{family:?}] {what}: expected a 23505 unique violation, got: {err}"),
        Ok(n) => panic!("[{family:?}] {what}: expected a 23505 unique violation, the statement succeeded ({n})"),
    }
}

fn assert_missing_table(result: heliosdb_nano::Result<Vec<Tuple>>, family: Family, what: &str) {
    match result {
        Err(err) if err.to_string().contains("does not exist") || err.to_string().contains("not found") => {}
        Err(err) => panic!("[{family:?}] {what}: expected 42P01 (no such table), got: {err}"),
        Ok(rows) => panic!(
            "[{family:?}] {what}: expected 42P01 (no such table), got {} rows",
            rows.len()
        ),
    }
}

// ---------------------------------------------------------------------------
// The raw keyspace: main's rows, main's catalog, the branch catalogs
// ---------------------------------------------------------------------------

/// FNV-1a over `bytes`, folded into `hash`.
fn fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// (key count, ordered checksum of keys and stored values) of every key whose
/// prefix is one of `prefixes`, read straight from RocksDB.
fn raw_state(db: &EmbeddedDatabase, prefixes: &[&str]) -> (usize, u64) {
    let raw = db.storage.db();
    let mut count = 0usize;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for prefix in prefixes {
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        for item in raw.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            read_opts,
        ) {
            let (key, value) = item.unwrap();
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            count += 1;
            hash = fold(hash, &key);
            hash = fold(hash, b"=");
            hash = fold(hash, &value);
            hash = fold(hash, b"|");
        }
    }
    (count, hash)
}

/// MAIN's rows of `table` (`data:{table}:`).
fn main_rows_state(db: &EmbeddedDatabase, table: &str) -> (usize, u64) {
    raw_state(db, &[&format!("data:{table}:")])
}

/// MAIN's catalog: every table, index, view, sequence, function, procedure,
/// trigger and constraint record.
fn main_catalog_state(db: &EmbeddedDatabase) -> (usize, u64) {
    raw_state(
        db,
        &[
            "meta:table:",
            "meta:table_incarnation:",
            "meta:index:",
            "meta:sequence:",
            "meta:seqstate:",
            "meta:function:",
            "meta:procedure:",
            "meta:identity:",
            "meta:schema:",
            "table_constraints:",
            "constraint:",
            "trigger:",
            "__view_metadata__",
        ],
    )
}

/// Every branch catalog record (`bmeta:`).
fn branch_catalog_state(db: &EmbeddedDatabase) -> (usize, u64) {
    raw_state(db, &["bmeta:"])
}

/// How many overlay keys (`bdata:` / `bdel:`) any level holds for `table`.
fn overlay_keys_of(db: &EmbeddedDatabase, table: &str) -> usize {
    let raw = db.storage.db();
    let mut count = 0usize;
    for prefix in ["bdata:", "bdel:"] {
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        for item in raw.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            read_opts,
        ) {
            let (key, _) = item.unwrap();
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let key = String::from_utf8_lossy(&key);
            let mut parts = key.splitn(4, ':');
            let (_, _, name) = (parts.next(), parts.next(), parts.next());
            if name == Some(table) {
                count += 1;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Introspection, through SQL
// ---------------------------------------------------------------------------

fn listed_tables(db: &EmbeddedDatabase, family: Family) -> Vec<String> {
    let mut names: Vec<String> = rows(
        db,
        family,
        "SELECT table_name, table_type FROM information_schema.tables",
        &[],
    )
    .iter()
    .filter(|row| text(row.values.get(1)).as_deref() == Some("BASE TABLE"))
    .filter_map(|row| text(row.values.first()))
    .collect();
    names.sort();
    names
}

fn listed_columns(db: &EmbeddedDatabase, family: Family, table: &str) -> Vec<String> {
    rows(
        db,
        family,
        "SELECT table_name, column_name FROM information_schema.columns",
        &[],
    )
    .iter()
    .filter(|row| text(row.values.first()).as_deref() == Some(table))
    .filter_map(|row| text(row.values.get(1)))
    .collect()
}

// ---------------------------------------------------------------------------
// Recipe 1 (migration rehearsal), 4a and 4b
// ---------------------------------------------------------------------------

fn seed_orders(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, amount INT)")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
}

/// Recipe 1 on the branch: the column exists there and the UPDATE applies;
/// main has no such column (catalog, introspection, `SELECT *`) and its rows
/// are byte-for-byte untouched.
fn rehearse(db: &EmbeddedDatabase, family: Family) {
    let rows_before = main_rows_state(db, "orders");
    let catalog_before = main_catalog_state(db);
    branch(db, "r");
    ok(
        db,
        family,
        "ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD'",
        &[],
    );
    ok(
        db,
        family,
        "UPDATE orders SET currency = $1 WHERE id = $2",
        &[s("EUR"), i(1)],
    );
    let seen: Vec<(i64, Option<String>)> = rows(db, family, "SELECT id, currency FROM orders ORDER BY id", &[])
        .iter()
        .map(|row| (int(row.values.first()), text(row.values.get(1))))
        .collect();
    assert_eq!(
        seen,
        vec![
            (1, Some("EUR".to_string())),
            (2, Some("USD".to_string())),
            (3, Some("USD".to_string()))
        ],
        "[{family:?}] the branch's column, backfilled with the default, and its UPDATE"
    );
    assert_eq!(
        listed_columns(db, family, "orders"),
        vec!["id", "amount", "currency"],
        "[{family:?}] the branch's introspection shows the column"
    );

    db.execute("USE BRANCH main").unwrap();
    assert_eq!(
        listed_columns(db, family, "orders"),
        vec!["id", "amount"],
        "[{family:?}] *** main's catalog grew the branch's column ***"
    );
    let main_rows = rows(db, family, "SELECT * FROM orders ORDER BY id", &[]);
    assert!(
        main_rows.iter().all(|row| row.values.len() == 2),
        "[{family:?}] *** main's rows were widened by the branch's ADD COLUMN ***"
    );
    assert_eq!(
        main_rows_state(db, "orders"),
        rows_before,
        "[{family:?}] *** main's rows changed ***"
    );
    assert_eq!(
        main_catalog_state(db),
        catalog_before,
        "[{family:?}] *** main's catalog changed ***"
    );
}

/// Recipe 1 + 4a: rehearse on a branch, discard it, then run the same
/// migration on main — which succeeds, because main never had the column.
#[test]
fn recipe_1_and_4a_rehearse_drop_then_migrate_main() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_orders(&db);
        rehearse(&db, family);
        db.execute("DROP BRANCH r").unwrap();
        ok(
            &db,
            family,
            "ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD'",
            &[],
        );
        let seen: Vec<Option<String>> = rows(&db, family, "SELECT currency FROM orders ORDER BY id", &[])
            .iter()
            .map(|row| text(row.values.first()))
            .collect();
        assert_eq!(
            seen,
            vec![Some("USD".into()), Some("USD".into()), Some("USD".into())],
            "[{family:?}] main's own migration, with main's backfill"
        );
    }
}

/// Recipe 1 + 4b: rehearse on a branch, then MERGE it into main — main gets
/// the column (same definition, same backfill) and the branch's values, and
/// later main inserts take the column's default.
#[test]
fn recipe_1_and_4b_rehearse_then_merge_into_main() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_orders(&db);
        rehearse(&db, family);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            listed_columns(&db, family, "orders"),
            vec!["id", "amount", "currency"],
            "[{family:?}] the merge carried the column into main's catalog"
        );
        let seen: Vec<(i64, Option<String>)> = rows(&db, family, "SELECT id, currency FROM orders ORDER BY id", &[])
            .iter()
            .map(|row| (int(row.values.first()), text(row.values.get(1))))
            .collect();
        assert_eq!(
            seen,
            vec![
                (1, Some("EUR".to_string())),
                (2, Some("USD".to_string())),
                (3, Some("USD".to_string()))
            ],
            "[{family:?}] main has the branch's values"
        );
        ok(
            &db,
            family,
            "INSERT INTO orders (id, amount) VALUES ($1, $2)",
            &[i(4), i(40)],
        );
        let four = rows(&db, family, "SELECT currency FROM orders WHERE id = $1", &[i(4)]);
        assert_eq!(text(four[0].values.first()).as_deref(), Some("USD"));
        // Main's PRIMARY KEY still holds over the merged rows.
        assert_unique_violation(
            exec(
                &db,
                family,
                "INSERT INTO orders (id, amount) VALUES ($1, $2)",
                &[i(1), i(1)],
            ),
            family,
            "a duplicate of a merged row on main",
        );
    }
}

// ---------------------------------------------------------------------------
// CREATE TABLE on a branch
// ---------------------------------------------------------------------------

/// A table created on a branch is invisible on main and on a sibling branch
/// (catalog AND introspection), its PRIMARY KEY and UNIQUE hold, it goes with
/// the branch, and when main later creates a table of the same name the
/// branch still sees only its own table and rows.
#[test]
fn create_table_on_a_branch_is_the_branchs_own() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE seed (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE BRANCH sibling AS OF NOW").unwrap();
        let catalog_before = main_catalog_state(&db);
        branch(&db, "a");
        ok(
            &db,
            family,
            "CREATE TABLE fresh (id INT PRIMARY KEY, code TEXT UNIQUE, n INT)",
            &[],
        );
        // IF NOT EXISTS over the branch's own table is a no-op.
        ok(&db, family, "CREATE TABLE IF NOT EXISTS fresh (other INT)", &[]);
        let ins = "INSERT INTO fresh VALUES ($1, $2, $3)";
        ok(&db, family, ins, &[i(1), s("c1"), i(10)]);
        ok(&db, family, ins, &[i(2), s("c2"), i(20)]);
        assert_unique_violation(
            exec(&db, family, ins, &[i(1), s("c9"), i(0)]),
            family,
            "PRIMARY KEY on a branch table",
        );
        assert_unique_violation(
            exec(&db, family, ins, &[i(3), s("c1"), i(0)]),
            family,
            "UNIQUE on a branch table",
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM fresh ORDER BY id", &[])),
            vec![1, 2]
        );
        assert!(listed_tables(&db, family).contains(&"fresh".to_string()));
        assert_eq!(listed_columns(&db, family, "fresh"), vec!["id", "code", "n"]);

        db.execute("USE BRANCH main").unwrap();
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM fresh", &[]),
            family,
            "the branch's table on main",
        );
        assert!(
            !listed_tables(&db, family).contains(&"fresh".to_string()),
            "[{family:?}] *** the branch's table is listed on main ***"
        );
        assert_eq!(
            main_catalog_state(&db),
            catalog_before,
            "[{family:?}] main's catalog changed"
        );
        assert_eq!(
            main_rows_state(&db, "fresh").0,
            0,
            "[{family:?}] main holds rows of fresh"
        );

        db.execute("USE BRANCH sibling").unwrap();
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM fresh", &[]),
            family,
            "the branch's table on a sibling",
        );
        assert!(!listed_tables(&db, family).contains(&"fresh".to_string()));

        // Main creates a table of the same name, with other columns and rows.
        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "CREATE TABLE fresh (id INT PRIMARY KEY, other TEXT)", &[]);
        ok(&db, family, "INSERT INTO fresh VALUES ($1, $2)", &[i(1), s("main")]);
        db.execute("USE BRANCH a").unwrap();
        let seen = rows(&db, family, "SELECT id, code FROM fresh ORDER BY id", &[]);
        assert_eq!(ids(&seen), vec![1, 2], "[{family:?}] the branch's own rows only");
        assert_eq!(text(seen[0].values.get(1)).as_deref(), Some("c1"));
        assert_eq!(listed_columns(&db, family, "fresh"), vec!["id", "code", "n"]);
        // The branch's own key 1 is still taken on the branch, main's row is
        // not a candidate for the branch's PRIMARY KEY.
        assert_unique_violation(exec(&db, family, ins, &[i(1), s("c5"), i(0)]), family, "own key");
        ok(&db, family, ins, &[i(5), s("c5"), i(50)]);

        db.execute("USE BRANCH main").unwrap();
        let main_seen = rows(&db, family, "SELECT id, other FROM fresh", &[]);
        assert_eq!(ids(&main_seen), vec![1]);
        assert_eq!(text(main_seen[0].values.get(1)).as_deref(), Some("main"));

        // Dropped with the branch: a new branch of the same name starts from
        // main's table, none of the old branch's.
        db.execute("DROP BRANCH a").unwrap();
        db.execute("DROP BRANCH sibling").unwrap();
        branch(&db, "a");
        assert_eq!(listed_columns(&db, family, "fresh"), vec!["id", "other"]);
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM fresh", &[])), vec![1]);
        db.execute("USE BRANCH main").unwrap();
    }
}

/// `CREATE TABLE … AS` on a branch: the table and its rows exist on the branch
/// only.
#[test]
fn create_table_as_on_a_branch_is_the_branchs_own() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_orders(&db);
        let catalog_before = main_catalog_state(&db);
        branch(&db, "b");
        ok(
            &db,
            family,
            "CREATE TABLE big AS SELECT id, amount FROM orders WHERE amount > 15",
            &[],
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM big ORDER BY id", &[])),
            vec![2, 3]
        );
        db.execute("USE BRANCH main").unwrap();
        assert_missing_table(try_rows(&db, family, "SELECT * FROM big", &[]), family, "CTAS on main");
        assert_eq!(
            main_catalog_state(&db),
            catalog_before,
            "[{family:?}] main's catalog changed"
        );
        assert_eq!(main_rows_state(&db, "big").0, 0);
        db.execute("DROP BRANCH b").unwrap();
    }
}

// ---------------------------------------------------------------------------
// DROP TABLE on a branch
// ---------------------------------------------------------------------------

/// `DROP TABLE` on a branch: main's table and rows are intact, the branch no
/// longer sees it (42P01), the branch's own rows of it are gone, and
/// recreating it on the branch starts empty. `DROP TABLE a, b` and `IF EXISTS`
/// likewise.
#[test]
fn drop_table_on_a_branch_leaves_main_intact() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE gone (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO gone VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .unwrap();
        db.execute("CREATE TABLE spare (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO spare VALUES (1)").unwrap();
        let rows_before = main_rows_state(&db, "gone");
        let spare_before = main_rows_state(&db, "spare");
        let catalog_before = main_catalog_state(&db);
        branch(&db, "b");
        // The branch's own rows of the table, to be dropped with it.
        ok(
            &db,
            family,
            "UPDATE gone SET v = $1 WHERE id = $2",
            &[s("branch"), i(1)],
        );
        ok(&db, family, "INSERT INTO gone VALUES ($1, $2)", &[i(4), s("d")]);
        assert!(
            overlay_keys_of(&db, "gone") > 0,
            "harness: the branch wrote rows of gone"
        );
        ok(&db, family, "DROP TABLE gone", &[]);
        assert_missing_table(try_rows(&db, family, "SELECT * FROM gone", &[]), family, "after DROP");
        assert!(!listed_tables(&db, family).contains(&"gone".to_string()));
        assert!(
            exec(&db, family, "DROP TABLE gone", &[]).is_err(),
            "[{family:?}] a second DROP"
        );
        ok(&db, family, "DROP TABLE IF EXISTS gone", &[]);
        assert_eq!(
            overlay_keys_of(&db, "gone"),
            0,
            "[{family:?}] the branch's own rows of the dropped table remain"
        );
        // Recreated on the branch: a new, empty table.
        ok(&db, family, "CREATE TABLE gone (id INT PRIMARY KEY, w INT)", &[]);
        assert!(rows(&db, family, "SELECT * FROM gone", &[]).is_empty());
        ok(&db, family, "INSERT INTO gone VALUES ($1, $2)", &[i(1), i(7)]);
        assert_eq!(listed_columns(&db, family, "gone"), vec!["id", "w"]);
        // The comma list.
        ok(&db, family, "DROP TABLE gone, spare", &[]);
        assert_missing_table(try_rows(&db, family, "SELECT * FROM spare", &[]), family, "DROP a, b");

        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            main_rows_state(&db, "gone"),
            rows_before,
            "[{family:?}] *** main's rows of gone changed ***"
        );
        assert_eq!(
            main_rows_state(&db, "spare"),
            spare_before,
            "[{family:?}] *** main's rows of spare changed ***"
        );
        assert_eq!(
            main_catalog_state(&db),
            catalog_before,
            "[{family:?}] *** main's catalog changed ***"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM gone ORDER BY id", &[])),
            vec![1, 2, 3]
        );
        assert_eq!(listed_columns(&db, family, "gone"), vec!["id", "v"]);
        db.execute("DROP BRANCH b").unwrap();
        assert_eq!(main_rows_state(&db, "gone"), rows_before);
    }
}

// ---------------------------------------------------------------------------
// ADD COLUMN on a branch
// ---------------------------------------------------------------------------

/// `ADD COLUMN … NOT NULL DEFAULT …` on a populated table: every inherited
/// row shows the default on the branch, inserts receive it (or supply their
/// own), NOT NULL and the PRIMARY KEY hold, and main is unchanged.
#[test]
fn add_column_with_a_default_on_a_branch() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)").unwrap();
        db.execute("INSERT INTO acct VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50)")
            .unwrap();
        let rows_before = main_rows_state(&db, "acct");
        let catalog_before = main_catalog_state(&db);
        branch(&db, "b");
        let rewritten = ok(
            &db,
            family,
            "ALTER TABLE acct ADD COLUMN status TEXT NOT NULL DEFAULT 'open'",
            &[],
        );
        assert_eq!(rewritten, 5, "[{family:?}] main's count: the rows rewritten");
        let statuses: Vec<Option<String>> = rows(&db, family, "SELECT status FROM acct ORDER BY id", &[])
            .iter()
            .map(|row| text(row.values.first()))
            .collect();
        assert_eq!(
            statuses,
            vec![Some("open".to_string()); 5],
            "[{family:?}] every inherited row"
        );
        ok(
            &db,
            family,
            "INSERT INTO acct (id, bal) VALUES ($1, $2)",
            &[i(6), i(60)],
        );
        ok(
            &db,
            family,
            "INSERT INTO acct VALUES ($1, $2, $3)",
            &[i(7), i(70), s("closed")],
        );
        let six = rows(&db, family, "SELECT status FROM acct WHERE id = $1", &[i(6)]);
        assert_eq!(
            text(six[0].values.first()).as_deref(),
            Some("open"),
            "[{family:?}] the default"
        );
        let seven = rows(&db, family, "SELECT status FROM acct WHERE id = $1", &[i(7)]);
        assert_eq!(text(seven[0].values.first()).as_deref(), Some("closed"));
        assert!(
            exec(
                &db,
                family,
                "INSERT INTO acct (id, bal, status) VALUES ($1, $2, $3)",
                &[i(8), i(80), Value::Null]
            )
            .is_err(),
            "[{family:?}] NOT NULL holds on the branch's column"
        );
        assert_unique_violation(
            exec(&db, family, "INSERT INTO acct (id, bal) VALUES ($1, $2)", &[i(1), i(0)]),
            family,
            "the PRIMARY KEY of the branch's definition",
        );
        // An existing column is refused, IF NOT EXISTS is a no-op.
        assert!(exec(&db, family, "ALTER TABLE acct ADD COLUMN status TEXT", &[]).is_err());
        assert_eq!(
            ok(
                &db,
                family,
                "ALTER TABLE acct ADD COLUMN IF NOT EXISTS status TEXT",
                &[]
            ),
            0
        );

        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            main_rows_state(&db, "acct"),
            rows_before,
            "[{family:?}] *** main's rows changed ***"
        );
        assert_eq!(
            main_catalog_state(&db),
            catalog_before,
            "[{family:?}] *** main's catalog changed ***"
        );
        assert_eq!(listed_columns(&db, family, "acct"), vec!["id", "bal"]);
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM acct ORDER BY id", &[])),
            vec![1, 2, 3, 4, 5]
        );
        db.execute("DROP BRANCH b").unwrap();
    }
}

/// Two branches each add a different column to the same table: each sees
/// only its own, main neither.
#[test]
fn two_branches_add_different_columns() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        branch(&db, "b1");
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 1", &[]);
        db.execute("USE BRANCH main").unwrap();
        branch(&db, "b2");
        ok(&db, family, "ALTER TABLE t ADD COLUMN y TEXT DEFAULT 'y'", &[]);
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v", "y"]);
        assert!(
            try_rows(&db, family, "SELECT x FROM t", &[]).is_err(),
            "[{family:?}] b1's column on b2"
        );
        db.execute("USE BRANCH b1").unwrap();
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v", "x"]);
        assert!(
            try_rows(&db, family, "SELECT y FROM t", &[]).is_err(),
            "[{family:?}] b2's column on b1"
        );
        let x = rows(&db, family, "SELECT x FROM t", &[]);
        assert_eq!(int(x[0].values.first()), 1);
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v"]);
        db.execute("DROP BRANCH b1").unwrap();
        db.execute("DROP BRANCH b2").unwrap();
    }
}

/// Branch of a branch: a child of `b` sees `b`'s new column and table; a DROP
/// TABLE in the child affects neither `b` nor main.
#[test]
fn a_child_branch_inherits_and_isolates_schema() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
        let rows_before = main_rows_state(&db, "t");
        branch(&db, "b");
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 5", &[]);
        ok(&db, family, "CREATE TABLE extra (id INT PRIMARY KEY, note TEXT)", &[]);
        ok(&db, family, "INSERT INTO extra VALUES ($1, $2)", &[i(1), s("from b")]);
        // A child of b (CREATE BRANCH without PARENT takes the session's).
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v", "x"]);
        let xs: Vec<i64> = rows(&db, family, "SELECT x FROM t ORDER BY id", &[])
            .iter()
            .map(|row| int(row.values.first()))
            .collect();
        assert_eq!(xs, vec![5, 5], "[{family:?}] b's column and backfill, inherited");
        let notes = rows(&db, family, "SELECT note FROM extra", &[]);
        assert_eq!(text(notes[0].values.first()).as_deref(), Some("from b"));
        ok(&db, family, "INSERT INTO extra VALUES ($1, $2)", &[i(2), s("from c")]);
        ok(&db, family, "DROP TABLE extra", &[]);
        ok(&db, family, "DROP TABLE t", &[]);
        assert_missing_table(try_rows(&db, family, "SELECT * FROM extra", &[]), family, "c's drop");
        assert_missing_table(try_rows(&db, family, "SELECT * FROM t", &[]), family, "c's drop of t");

        db.execute("USE BRANCH b").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM extra", &[])),
            vec![1],
            "[{family:?}] b's table"
        );
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v", "x"]);
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM t ORDER BY id", &[])), vec![1, 2]);
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            main_rows_state(&db, "t"),
            rows_before,
            "[{family:?}] *** main's rows changed ***"
        );
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v"]);
        db.execute("DROP BRANCH c").unwrap();
        db.execute("DROP BRANCH b").unwrap();
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Every schema statement a branch does not scope is refused (0A000) before it
/// changes anything: main's catalog, the branch catalogs and the branch's view
/// of the table are what they were.
#[test]
fn other_schema_statements_are_refused_on_a_branch() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT, w INT)").unwrap();
        db.execute("CREATE INDEX r_w_idx ON r (w)").unwrap();
        db.execute("INSERT INTO r VALUES (1, 1, 1)").unwrap();
        db.execute("CREATE SEQUENCE ms").unwrap();
        db.execute("CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$")
            .unwrap();
        branch(&db, "b");
        let main_catalog = main_catalog_state(&db);
        let branch_catalog = branch_catalog_state(&db);
        for sql in [
            "ALTER TABLE r DROP COLUMN w",
            "ALTER TABLE r RENAME COLUMN v TO vv",
            "ALTER TABLE r RENAME TO r2",
            "ALTER TABLE r ALTER COLUMN v DROP NOT NULL",
            "CREATE INDEX r_v_idx ON r (v)",
            "DROP INDEX r_w_idx",
            "ALTER TABLE r ADD CONSTRAINT r_v_key UNIQUE (v)",
            "ALTER TABLE r ADD CONSTRAINT r_v_check CHECK (v > 0)",
            "ALTER TABLE r ADD COLUMN u INT UNIQUE",
            "ALTER TABLE r ADD COLUMN z INT, DROP COLUMN w",
            "CREATE VIEW rv AS SELECT id FROM r",
            "CREATE SEQUENCE rs",
            "CREATE FUNCTION one() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql",
            "CREATE TEMPORARY TABLE scratch (id INT)",
            "CREATE TRIGGER trg BEFORE INSERT ON r FOR EACH ROW EXECUTE FUNCTION f()",
            "GRANT SELECT ON r TO PUBLIC",
            "CREATE TYPE mood AS ENUM ('a', 'b')",
            "CREATE EXTENSION hdb_code",
        ] {
            assert_refused_on_branch(exec(&db, family, sql, &[]), family, sql);
            assert_eq!(
                main_catalog_state(&db),
                main_catalog,
                "[{family:?}] {sql}: *** main's catalog changed ***"
            );
            assert_eq!(
                branch_catalog_state(&db),
                branch_catalog,
                "[{family:?}] {sql}: the branch catalog changed"
            );
            assert_eq!(
                listed_columns(&db, family, "r"),
                vec!["id", "v", "w"],
                "[{family:?}] {sql}: the branch's table changed"
            );
        }
        // The text family's own pre-parses: it splits a schema's elements and reads
        // `ALTER SEQUENCE` itself (sqlparser has no grammar for either). The params
        // family has no such step, so these fail there with a parse error, and
        // never reach anything that could change a catalog.
        for sql in [
            "CREATE SCHEMA s CREATE TABLE s.x (id INT)",
            "ALTER SEQUENCE ms RESTART WITH 5",
        ] {
            if family == Family::Text {
                assert_refused_on_branch(exec(&db, family, sql, &[]), family, sql);
            } else {
                assert!(exec(&db, family, sql, &[]).is_err(), "[{family:?}] {sql}");
            }
            assert_eq!(main_catalog_state(&db), main_catalog, "[{family:?}] {sql}");
            assert_eq!(branch_catalog_state(&db), branch_catalog, "[{family:?}] {sql}");
        }
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(listed_columns(&db, family, "r"), vec!["id", "v", "w"]);
        db.execute("DROP BRANCH b").unwrap();
    }
}

// ---------------------------------------------------------------------------
// MERGE BRANCH with schema changes
// ---------------------------------------------------------------------------

/// MERGE is refused (55000) — before anything is written — when main changed
/// the definition of a table the branch changed, after the branch did; the
/// branch stays active and intact.
#[test]
fn merge_is_refused_when_main_changed_an_affected_table() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE m (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO m VALUES (1, 10)").unwrap();
        branch(&db, "b");
        ok(&db, family, "ALTER TABLE m ADD COLUMN x INT DEFAULT 1", &[]);
        ok(&db, family, "UPDATE m SET v = $1 WHERE id = $2", &[i(99), i(1)]);
        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "ALTER TABLE m ADD COLUMN y INT DEFAULT 2", &[]);
        let rows_before = main_rows_state(&db, "m");
        let catalog_before = main_catalog_state(&db);
        // (MERGE BRANCH is branch syntax the text family pre-parses; the
        // schema step it refuses ran on the family under test above.)
        match db.execute("MERGE BRANCH b INTO main") {
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER) && message.contains("definition of m "),
                    "[{family:?}] expected the 55000 schema refusal naming m, got: {message}"
                );
            }
            Ok(n) => panic!("[{family:?}] the merge went through ({n})"),
        }
        assert_eq!(
            main_rows_state(&db, "m"),
            rows_before,
            "[{family:?}] *** the refused merge wrote rows ***"
        );
        assert_eq!(
            main_catalog_state(&db),
            catalog_before,
            "[{family:?}] *** the refused merge changed the catalog ***"
        );
        assert_eq!(listed_columns(&db, family, "m"), vec!["id", "v", "y"]);
        // The branch is still there, as it was.
        db.execute("USE BRANCH b").unwrap();
        assert_eq!(listed_columns(&db, family, "m"), vec!["id", "v", "x"]);
        let v = rows(&db, family, "SELECT v FROM m", &[]);
        assert_eq!(int(v[0].values.first()), 99);
        db.execute("USE BRANCH main").unwrap();
        db.execute("DROP BRANCH b").unwrap();
    }
}

/// MERGE carries a branch's CREATE TABLE (definition, rows, constraints) and
/// its DROP TABLE into main.
#[test]
fn merge_carries_created_and_dropped_tables_into_main() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE doomed (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO doomed VALUES (1)").unwrap();
        branch(&db, "b");
        ok(
            &db,
            family,
            "CREATE TABLE born (id INT PRIMARY KEY, code TEXT UNIQUE)",
            &[],
        );
        ok(&db, family, "INSERT INTO born VALUES ($1, $2)", &[i(1), s("one")]);
        ok(&db, family, "INSERT INTO born VALUES ($1, $2)", &[i(2), s("two")]);
        ok(&db, family, "DROP TABLE doomed", &[]);
        db.execute("USE BRANCH main").unwrap();
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM born", &[]),
            family,
            "before the merge",
        );
        db.execute("MERGE BRANCH b INTO main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM born ORDER BY id", &[])),
            vec![1, 2]
        );
        // The DURABLE counter was raised above the merged row ids: `create_table`
        // writes 0, so a missing raise would hand them out again after a restart.
        let mut largest = 0u64;
        let raw = db.storage.db();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        for item in raw.iterator_opt(
            rocksdb::IteratorMode::From(b"data:born:", rocksdb::Direction::Forward),
            read_opts,
        ) {
            let (key, _) = item.unwrap();
            let Some(row_id) = key.strip_prefix(b"data:born:".as_slice()) else {
                break;
            };
            largest = largest.max(String::from_utf8_lossy(row_id).parse().unwrap());
        }
        let counter = raw.get(b"counter:born").unwrap().expect("the durable row counter");
        let counter: u64 = bincode::deserialize(&counter).unwrap();
        assert!(
            largest > 0 && counter >= largest,
            "[{family:?}] counter {counter}, largest merged row id {largest}"
        );
        assert_eq!(listed_columns(&db, family, "born"), vec!["id", "code"]);
        assert_unique_violation(
            exec(&db, family, "INSERT INTO born VALUES ($1, $2)", &[i(3), s("one")]),
            family,
            "the merged table's UNIQUE on main",
        );
        ok(&db, family, "INSERT INTO born VALUES ($1, $2)", &[i(3), s("three")]);
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM born ORDER BY id", &[])),
            vec![1, 2, 3],
            "[{family:?}] the new row overwrote nothing"
        );
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM doomed", &[]),
            family,
            "merged DROP",
        );
        assert_eq!(
            main_rows_state(&db, "doomed").0,
            0,
            "[{family:?}] the dropped table's rows"
        );
    }
}

// ---------------------------------------------------------------------------
// The PostgreSQL wire: SQLSTATEs and Describe
// ---------------------------------------------------------------------------

struct Server {
    db: Arc<EmbeddedDatabase>,
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn serve(db: EmbeddedDatabase) -> Server {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    Server {
        db,
        port: addr.port(),
        listener,
    }
}

async fn connect(port: u16) -> (Client, tokio::task::JoinHandle<()>) {
    let cs = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres");
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    (client, task)
}

fn sqlstate(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.code().code().to_string())
        .unwrap_or_else(|| format!("<not a DbError: {e}>"))
}

/// Over the wire: a branch session's extended-protocol Describe reports the
/// branch's definition (its ADD COLUMN), a main session's reports main's; a
/// refused statement is 0A000, a dropped table 42P01, a refused MERGE 55000.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_sqlstates_and_describe_on_a_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);
    db.execute("CREATE TABLE side (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO side VALUES (1, 1)").unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    let orders_before = main_rows_state(&db, "orders");
    let s = serve(db).await;
    let (on_branch, branch_task) = connect(s.port).await;
    let (on_main, main_task) = connect(s.port).await;
    on_branch.simple_query("USE BRANCH r").await.unwrap();

    on_branch
        .execute(
            "ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD'",
            &[],
        )
        .await
        .expect("ADD COLUMN on a branch over the extended protocol");
    let prepared = on_branch.prepare("SELECT * FROM orders ORDER BY id").await.unwrap();
    assert_eq!(prepared.columns().len(), 3, "the branch session's Describe");
    let rows = on_branch.query(&prepared, &[]).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get::<_, String>(2), "USD");
    let prepared_main = on_main.prepare("SELECT * FROM orders ORDER BY id").await.unwrap();
    assert_eq!(prepared_main.columns().len(), 2, "the main session's Describe");
    assert_eq!(on_main.query(&prepared_main, &[]).await.unwrap().len(), 3);

    let err = on_branch
        .execute("ALTER TABLE orders DROP COLUMN amount", &[])
        .await
        .err()
        .expect("DROP COLUMN on a branch");
    assert_eq!(sqlstate(&err), "0A000");
    let err = on_branch
        .simple_query("CREATE VIEW ov AS SELECT id FROM orders")
        .await
        .err()
        .expect("CREATE VIEW on a branch");
    assert_eq!(sqlstate(&err), "0A000");

    on_branch.simple_query("DROP TABLE orders").await.unwrap();
    let err = on_branch
        .query("SELECT * FROM orders", &[])
        .await
        .err()
        .expect("a dropped table");
    assert_eq!(sqlstate(&err), "42P01");
    assert_eq!(on_main.query("SELECT id FROM orders", &[]).await.unwrap().len(), 3);

    // A refused MERGE: main changed `side` after the branch did.
    on_branch
        .simple_query("ALTER TABLE side ADD COLUMN x INT DEFAULT 1")
        .await
        .unwrap();
    on_main
        .simple_query("ALTER TABLE side ADD COLUMN y INT DEFAULT 2")
        .await
        .unwrap();
    let err = on_main
        .simple_query("MERGE BRANCH r INTO main")
        .await
        .err()
        .expect("a merge over a moved table");
    assert_eq!(sqlstate(&err), "55000");

    drop(on_branch);
    drop(on_main);
    branch_task.abort();
    main_task.abort();
    assert_eq!(
        main_rows_state(&s.db, "orders"),
        orders_before,
        "*** main's rows of orders changed ***"
    );
}

// ---------------------------------------------------------------------------
// B3 fix pass: what MERGE carries, and what it refuses
// ---------------------------------------------------------------------------

/// How many keys start with `prefix`, read straight from RocksDB.
fn count_keys(db: &EmbeddedDatabase, prefix: &str) -> usize {
    raw_state(db, &[prefix]).0
}

fn branch_is_active(db: &EmbeddedDatabase, name: &str) -> bool {
    db.storage.get_branch(name).unwrap().state == BranchState::Active
}

/// `orders` (ids 1..=3) forked to branch `r`, which adds `currency`.
fn seed_and_add_currency(db: &EmbeddedDatabase, family: Family) {
    seed_orders(db);
    branch(db, "r");
    ok(
        db,
        family,
        "ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD'",
        &[],
    );
}

fn order_rows(db: &EmbeddedDatabase, family: Family) -> Vec<(i64, i64, Option<String>)> {
    rows(db, family, "SELECT id, amount, currency FROM orders ORDER BY id", &[])
        .iter()
        .map(|row| (int(row.values.first()), int(row.values.get(1)), text(row.values.get(2))))
        .collect()
}

fn usd(id: i64, amount: i64) -> (i64, i64, Option<String>) {
    (id, amount, Some("USD".to_string()))
}

/// The materialized rows of a branch's ADD COLUMN are copies, not changes:
/// every row main changed after the fork keeps main's change.
#[test]
fn merge_keeps_mains_row_changes_made_after_the_branchs_add_column() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_and_add_currency(&db, family);
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1, currency = $2 WHERE id = $3",
            &[i(11), s("EUR"), i(1)],
        );
        db.execute("USE BRANCH main").unwrap();
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1 WHERE id = $2",
            &[i(222), i(2)],
        );
        ok(&db, family, "DELETE FROM orders WHERE id = $1", &[i(3)]);
        ok(&db, family, "INSERT INTO orders VALUES ($1, $2)", &[i(4), i(40)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            order_rows(&db, family),
            vec![(1, 11, Some("EUR".to_string())), usd(2, 222), usd(4, 40)],
            "[{family:?}] main's own changes survive, the branch's change lands"
        );
    }
}

/// Main emptied the table after the fork: only what the branch itself changed
/// comes back.
#[test]
fn merge_after_main_truncated_brings_back_only_the_branchs_changes() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_and_add_currency(&db, family);
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1 WHERE id = $2",
            &[i(11), i(1)],
        );
        db.execute("USE BRANCH main").unwrap();
        db.execute("TRUNCATE TABLE orders").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(order_rows(&db, family), vec![usd(1, 11)], "[{family:?}]");
    }
}

/// On the raw keyspace: the materialization marks the rows the level did not
/// already hold, and the level's own write of a row clears its mark.
#[test]
fn add_column_marks_copies_and_an_own_write_clears_the_mark() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_orders(&db);
        branch(&db, "r");
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1 WHERE id = $2",
            &[i(11), i(1)],
        );
        ok(&db, family, "ALTER TABLE orders ADD COLUMN c INT DEFAULT 1", &[]);
        assert_eq!(
            count_keys(&db, "bfrz:"),
            2,
            "[{family:?}] rows 2 and 3 are copies, row 1 is the branch's own change"
        );
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1 WHERE id = $2",
            &[i(22), i(2)],
        );
        assert_eq!(count_keys(&db, "bfrz:"), 1, "[{family:?}] an UPDATE clears the mark");
        ok(&db, family, "DELETE FROM orders WHERE id = $1", &[i(3)]);
        assert_eq!(count_keys(&db, "bfrz:"), 0, "[{family:?}] a DELETE clears the mark");
    }
}

/// A created table, a table with a new column and a changed row, and a dropped
/// table on branch `r`, on main's `b_added` and `c_gone`.
fn seed_merge_of_three_steps(db: &EmbeddedDatabase, family: Family) {
    db.execute("CREATE TABLE b_added (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO b_added VALUES (1, 1), (2, 2)").unwrap();
    db.execute("CREATE TABLE c_gone (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO c_gone VALUES (1)").unwrap();
    branch(db, "r");
    ok(
        db,
        family,
        "CREATE TABLE a_new (id INT PRIMARY KEY, note TEXT UNIQUE)",
        &[],
    );
    ok(db, family, "INSERT INTO a_new VALUES ($1, $2)", &[i(1), s("new")]);
    ok(db, family, "ALTER TABLE b_added ADD COLUMN x INT DEFAULT 7", &[]);
    ok(db, family, "UPDATE b_added SET v = $1 WHERE id = $2", &[i(10), i(1)]);
    ok(db, family, "DROP TABLE c_gone", &[]);
    db.execute("USE BRANCH main").unwrap();
}

fn assert_merge_of_three_steps_finished(db: &EmbeddedDatabase, family: Family) {
    assert_eq!(count_keys(db, "branch:merge:"), 0, "[{family:?}] the intent is gone");
    assert!(
        matches!(db.storage.get_branch("r").unwrap().state, BranchState::Merged { .. }),
        "[{family:?}] the source is merged"
    );
    assert_eq!(
        ids(&rows(db, family, "SELECT id FROM a_new", &[])),
        vec![1],
        "[{family:?}]"
    );
    assert_unique_violation(
        exec(db, family, "INSERT INTO a_new VALUES ($1, $2)", &[i(2), s("new")]),
        family,
        "the created table's UNIQUE on main",
    );
    let added = rows(db, family, "SELECT id, v, x FROM b_added ORDER BY id", &[]);
    let added: Vec<(i64, i64, i64)> = added
        .iter()
        .map(|row| (int(row.values.first()), int(row.values.get(1)), int(row.values.get(2))))
        .collect();
    assert_eq!(added, vec![(1, 10, 7), (2, 2, 7)], "[{family:?}]");
    assert_missing_table(try_rows(db, family, "SELECT * FROM c_gone", &[]), family, "merged DROP");
}

/// A MERGE that stops half way refuses what would disturb it (main's DDL on its
/// tables, a DROP or a re-CREATE of its source), and running it again finishes
/// it. Rolled forward, never back.
#[test]
fn merge_failure_mid_schema_resumes_on_rerun() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_merge_of_three_steps(&db, family);
        db.storage.debug_fail_merge_after_steps(1);
        let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");
        assert_eq!(
            count_keys(&db, "branch:merge:"),
            1,
            "[{family:?}] the intent is durable"
        );
        for refused in [
            exec(&db, family, "ALTER TABLE b_added ADD COLUMN q INT", &[]),
            exec(&db, family, "DROP TABLE c_gone", &[]),
            db.execute("DROP BRANCH r"),
            db.execute("CREATE BRANCH r AS OF NOW"),
        ] {
            let err = refused.expect_err("an unfinished merge refuses it").to_string();
            assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");
        }
        // A table the merge does not change is free.
        ok(&db, family, "CREATE TABLE unrelated (id INT)", &[]);

        // Run it again; a second stop on the way (before the step that stopped it
        // the first time) changes nothing about how it ends.
        db.storage.debug_fail_merge_after_steps(0);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_merge_of_three_steps_finished(&db, family);
        // The merge's tables are main's own again.
        ok(&db, family, "ALTER TABLE b_added ADD COLUMN q INT", &[]);
    }
}

/// A process that died mid-merge: the next open finishes it.
#[test]
fn merge_interrupted_then_reopened_is_finished_at_open() {
    for family in FAMILIES {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = EmbeddedDatabase::new(dir.path()).unwrap();
            seed_merge_of_three_steps(&db, family);
            db.storage.debug_fail_merge_after_steps(1);
            let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
            assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");
        }
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        assert_merge_of_three_steps_finished(&db, family);
    }
}

/// A failure between an ADD COLUMN's schema update and its backfill, twice: every
/// row ends up exactly one column wider, in storage.
#[test]
fn merge_failure_inside_add_column_backfill_is_idempotent() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_and_add_currency(&db, family);
        ok(
            &db,
            family,
            "UPDATE orders SET amount = $1 WHERE id = $2",
            &[i(11), i(1)],
        );
        db.execute("USE BRANCH main").unwrap();
        for _ in 0..2 {
            db.storage.debug_fail_merge_after_steps(1);
            let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
            assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");
        }
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            order_rows(&db, family),
            vec![usd(1, 11), usd(2, 20), usd(3, 30)],
            "[{family:?}]"
        );
        let raw = db.storage.db();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let prefix = b"data:orders:";
        for item in raw.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        ) {
            let (key, value) = item.unwrap();
            if !key.starts_with(prefix) {
                break;
            }
            let tuple: Tuple = bincode::deserialize(&value).unwrap();
            assert_eq!(tuple.values.len(), 3, "[{family:?}] widened exactly once");
        }
    }
}

/// `MERGE c INTO b` when c added a column: b takes the definition, keeps every
/// row it saw (a marked copy, unless it was b's own), and gets c's changes. A
/// later `MERGE b INTO main` then carries b's and c's changes, not the copies.
#[test]
fn merge_into_a_branch_carries_add_column_with_backfill() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
        branch(&db, "b");
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        ok(
            &db,
            family,
            "ALTER TABLE t ADD COLUMN cur TEXT NOT NULL DEFAULT 'USD'",
            &[],
        );
        ok(&db, family, "UPDATE t SET cur = $1 WHERE id = $2", &[s("EUR"), i(1)]);
        // A key no materialization on b ever sees: only the row phase can index it.
        ok(
            &db,
            family,
            "INSERT INTO t VALUES ($1, $2, $3)",
            &[i(20), i(200), s("GBP")],
        );
        db.execute("USE BRANCH b").unwrap();
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(10), i(100)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH c INTO b").unwrap();

        db.execute("USE BRANCH b").unwrap();
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v", "cur"]);
        let on_b: Vec<(i64, Option<String>)> = rows(&db, family, "SELECT id, cur FROM t ORDER BY id", &[])
            .iter()
            .map(|row| (int(row.values.first()), text(row.values.get(1))))
            .collect();
        assert_eq!(
            on_b,
            vec![
                (1, Some("EUR".to_string())),
                (2, Some("USD".to_string())),
                (10, Some("USD".to_string())),
                (20, Some("GBP".to_string()))
            ],
            "[{family:?}] b's view, under the new definition"
        );
        assert_unique_violation(
            exec(&db, family, "INSERT INTO t VALUES ($1, $2, $3)", &[i(10), i(0), s("x")]),
            family,
            "the target's key index holds its rows",
        );
        assert_unique_violation(
            exec(&db, family, "INSERT INTO t VALUES ($1, $2, $3)", &[i(20), i(0), s("x")]),
            family,
            "the target's key index holds the merged row",
        );

        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(222), i(2)]);
        db.execute("MERGE BRANCH b INTO main").unwrap();
        let on_main: Vec<(i64, i64, Option<String>)> = rows(&db, family, "SELECT id, v, cur FROM t ORDER BY id", &[])
            .iter()
            .map(|row| (int(row.values.first()), int(row.values.get(1)), text(row.values.get(2))))
            .collect();
        assert_eq!(
            on_main,
            vec![
                (1, 1, Some("EUR".to_string())),
                (2, 222, Some("USD".to_string())),
                (10, 100, Some("USD".to_string())),
                (20, 200, Some("GBP".to_string()))
            ],
            "[{family:?}] main keeps its own change of a row that was a copy on b"
        );
    }
}

/// An isolated table's constraints are served by the level's durable key index,
/// which follows every write and every ROLLBACK.
#[test]
fn isolated_table_constraints_use_the_branch_key_index() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE m (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO m VALUES (1, 1), (2, 2)").unwrap();
        branch(&db, "r");
        ok(
            &db,
            family,
            "CREATE TABLE k (id INT PRIMARY KEY, code TEXT UNIQUE, n INT)",
            &[],
        );
        let ins = "INSERT INTO k VALUES ($1, $2, $3)";
        for (id, code) in [(1, "c1"), (2, "c2"), (3, "c3")] {
            ok(&db, family, ins, &[i(id), s(code), i(0)]);
        }
        assert_eq!(count_keys(&db, "bkey:"), 6, "[{family:?}] two keys per row");
        assert_unique_violation(exec(&db, family, ins, &[i(1), s("c9"), i(0)]), family, "PK");
        assert_unique_violation(exec(&db, family, ins, &[i(9), s("c1"), i(0)]), family, "UNIQUE");

        ok(&db, family, "DELETE FROM k WHERE id = $1", &[i(2)]);
        assert_eq!(count_keys(&db, "bkey:"), 4, "[{family:?}] a DELETE frees its keys");
        ok(&db, family, ins, &[i(4), s("c2"), i(0)]);
        ok(&db, family, "UPDATE k SET code = $1 WHERE id = $2", &[s("moved"), i(1)]);
        assert_eq!(count_keys(&db, "bkey:"), 6, "[{family:?}] an UPDATE moves its key");
        ok(&db, family, ins, &[i(5), s("c1"), i(0)]);
        assert_unique_violation(exec(&db, family, ins, &[i(6), s("moved"), i(0)]), family, "moved key");

        ok(&db, family, "BEGIN", &[]);
        ok(&db, family, ins, &[i(7), s("c7"), i(0)]);
        assert_unique_violation(
            exec(&db, family, ins, &[i(8), s("c7"), i(0)]),
            family,
            "a key the transaction staged",
        );
        ok(&db, family, "ROLLBACK", &[]);
        assert_eq!(count_keys(&db, "bkey:"), 8, "[{family:?}] ROLLBACK discards its keys");
        ok(&db, family, ins, &[i(7), s("c7"), i(0)]);
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM k ORDER BY id", &[])),
            vec![1, 3, 4, 5, 7]
        );

        // A table made isolated by ADD COLUMN: its materialized rows are indexed.
        let before = count_keys(&db, "bkey:");
        ok(&db, family, "ALTER TABLE m ADD COLUMN w INT", &[]);
        assert_eq!(count_keys(&db, "bkey:"), before + 2, "[{family:?}]");
        assert_unique_violation(
            exec(&db, family, "INSERT INTO m VALUES ($1, $2, $3)", &[i(2), i(0), i(0)]),
            family,
            "a materialized row's key",
        );
        ok(&db, family, "DROP TABLE k", &[]);
        assert_eq!(
            count_keys(&db, "bkey:"),
            2,
            "[{family:?}] a DROP clears the table's index"
        );
    }
}

/// A branch's reverse-FK answer is cached, and its DDL invalidates it.
#[test]
fn a_branch_reverse_fk_answer_follows_its_ddl() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO parent VALUES (1)").unwrap();
        branch(&db, "r");
        ok(
            &db,
            family,
            "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
            &[],
        );
        ok(&db, family, "INSERT INTO child VALUES ($1, $2)", &[i(1), i(1)]);
        for _ in 0..2 {
            assert!(
                exec(&db, family, "DELETE FROM parent WHERE id = $1", &[i(1)]).is_err(),
                "[{family:?}] the branch's child references the row"
            );
        }
        ok(&db, family, "DROP TABLE child", &[]);
        ok(&db, family, "DELETE FROM parent WHERE id = $1", &[i(1)]);
        db.execute("USE BRANCH main").unwrap();
        assert!(
            db.storage.catalog().get_referencing_fks("parent").unwrap().is_empty(),
            "[{family:?}] main never had the branch's foreign key"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM parent", &[])),
            vec![1],
            "[{family:?}] main's row"
        );
    }
}

/// A branch with live children refuses its DDL (0A000): they read through it,
/// and freezing what they see is phase B4. After the child is gone it works.
#[test]
fn ddl_on_a_branch_with_live_children_is_refused() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
        branch(&db, "b");
        ok(&db, family, "CREATE TABLE own (id INT PRIMARY KEY)", &[]);
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        let catalog_before = branch_catalog_state(&db);
        for sql in [
            "DROP TABLE own",
            "CREATE TABLE fresh (id INT)",
            "CREATE TABLE fresh2 AS SELECT id FROM t",
            "ALTER TABLE t ADD COLUMN x INT",
        ] {
            let err = exec(&db, family, sql, &[]).expect_err(sql).to_string();
            assert!(
                err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER) && err.contains("child branches"),
                "[{family:?}] {sql}: {err}"
            );
            assert_eq!(
                branch_catalog_state(&db),
                catalog_before,
                "[{family:?}] {sql}: nothing changed"
            );
        }
        // A no-op is not a change.
        ok(&db, family, "CREATE TABLE IF NOT EXISTS own (other INT)", &[]);
        db.execute("USE BRANCH c").unwrap();
        assert_eq!(listed_columns(&db, family, "t"), vec!["id", "v"]);
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM own", &[])), Vec::<i64>::new());
        db.execute("USE BRANCH b").unwrap();
        db.execute("DROP BRANCH c").unwrap();
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]);
        ok(&db, family, "DROP TABLE own", &[]);
        ok(&db, family, "CREATE TABLE fresh (id INT)", &[]);
    }
}

/// A schema-changing MERGE into a branch with OTHER live children is refused too.
#[test]
fn a_schema_changing_merge_into_a_branch_with_other_children_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    branch(&db, "b");
    db.execute("CREATE BRANCH c1 AS OF NOW").unwrap();
    db.execute("CREATE BRANCH c2 AS OF NOW").unwrap();
    db.execute("USE BRANCH c1").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN x INT").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let err = db.execute("MERGE BRANCH c1 INTO b").unwrap_err().to_string();
    assert!(
        err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER) && err.contains("c2"),
        "{err}"
    );
    assert!(branch_is_active(&db, "c1"), "the source stays mergeable");
    assert_eq!(count_keys(&db, "branch:merge:"), 0);
    db.execute("DROP BRANCH c2").unwrap();
    db.execute("MERGE BRANCH c1 INTO b").unwrap();
}

/// `pg_indexes` on a branch: main's index of a table the branch dropped is gone,
/// the branch's own table's PRIMARY KEY is there; main is as it was.
#[test]
fn pg_indexes_on_a_branch_lists_the_branchs_tables() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE gone (id INT PRIMARY KEY, w INT)").unwrap();
        db.execute("CREATE INDEX gone_w_idx ON gone (w)").unwrap();
        branch(&db, "r");
        ok(&db, family, "DROP TABLE gone", &[]);
        ok(&db, family, "CREATE TABLE fresh (id INT PRIMARY KEY)", &[]);
        let indexes = |db: &EmbeddedDatabase| -> Vec<String> {
            rows(db, family, "SELECT indexname FROM pg_indexes", &[])
                .iter()
                .filter_map(|row| text(row.values.first()))
                .collect()
        };
        let on_branch = indexes(&db);
        assert!(
            !on_branch.contains(&"gone_w_idx".to_string()),
            "[{family:?}] {on_branch:?}"
        );
        assert!(
            on_branch.contains(&"fresh_pkey".to_string()),
            "[{family:?}] {on_branch:?}"
        );
        db.execute("USE BRANCH main").unwrap();
        let on_main = indexes(&db);
        assert!(on_main.contains(&"gone_w_idx".to_string()), "[{family:?}] {on_main:?}");
        assert!(!on_main.contains(&"fresh_pkey".to_string()), "[{family:?}] {on_main:?}");
    }
}

/// A `CREATE UNIQUE INDEX` on main after the fork changes the definition: 55000
/// before the merge, rather than a duplicate afterwards.
#[test]
fn merge_refused_when_main_adds_a_unique_index_after_the_fork() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE u (id INT PRIMARY KEY, v INT)").unwrap();
        branch(&db, "r");
        ok(&db, family, "ALTER TABLE u ADD COLUMN x INT", &[]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("CREATE UNIQUE INDEX u_v_key ON u (v)").unwrap();
        let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER), "[{family:?}] {err}");
        assert!(branch_is_active(&db, "r"), "[{family:?}]");
    }
}

/// A table a branch created and dropped is no change: main's later table of the
/// name is left alone (and the branch still does not see it).
#[test]
fn merge_of_a_branch_that_created_and_dropped_a_table_ignores_mains_later_table() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        branch(&db, "r");
        ok(&db, family, "CREATE TABLE later (id INT PRIMARY KEY)", &[]);
        ok(&db, family, "DROP TABLE later", &[]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("CREATE TABLE later (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO later VALUES (1)").unwrap();
        let before = main_rows_state(&db, "later");
        db.execute("USE BRANCH r").unwrap();
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM later", &[]),
            family,
            "the branch's tombstone holds",
        );
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(main_rows_state(&db, "later"), before, "[{family:?}]");
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM later", &[])), vec![1]);
    }
}

/// A branch's own table never consults main's ART index of the name.
#[test]
fn a_branch_table_is_not_checked_against_mains_index_of_the_name() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        branch(&db, "r");
        ok(&db, family, "CREATE TABLE fresh (id INT PRIMARY KEY)", &[]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("CREATE TABLE fresh (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO fresh VALUES (7)").unwrap();
        db.execute("USE BRANCH r").unwrap();
        ok(&db, family, "INSERT INTO fresh VALUES ($1)", &[i(7)]);
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM fresh", &[])), vec![7]);
    }
}

// ---------------------------------------------------------------------------
// B3 fix pass, amendment A: table incarnations
// ---------------------------------------------------------------------------

fn raw_key(db: &EmbeddedDatabase, key: &str) -> Option<Vec<u8>> {
    db.storage.db().get(key.as_bytes()).unwrap()
}

/// `CREATE` mints an id, `RENAME` moves it, `DROP` deletes it; a later `CREATE`
/// of the name is another table.
#[test]
fn a_table_incarnation_follows_create_rename_and_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    let first = raw_key(&db, "meta:table_incarnation:t").expect("CREATE TABLE mints an id");
    db.execute("ALTER TABLE t RENAME TO u").unwrap();
    assert_eq!(raw_key(&db, "meta:table_incarnation:t"), None);
    assert_eq!(raw_key(&db, "meta:table_incarnation:u").as_ref(), Some(&first));
    db.execute("DROP TABLE u").unwrap();
    assert_eq!(raw_key(&db, "meta:table_incarnation:u"), None);
    db.execute("CREATE TABLE u (id INT PRIMARY KEY)").unwrap();
    assert_ne!(raw_key(&db, "meta:table_incarnation:u").as_ref(), Some(&first));
}

/// A table from before incarnation ids gets one at the next writable open.
#[test]
fn a_table_with_no_incarnation_gets_one_at_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE legacy (id INT PRIMARY KEY)").unwrap();
        db.storage.delete(&b"meta:table_incarnation:legacy".to_vec()).unwrap();
        assert_eq!(raw_key(&db, "meta:table_incarnation:legacy"), None);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(raw_key(&db, "meta:table_incarnation:legacy").is_some());
}

/// `DROP TABLE t` + an identical `CREATE TABLE t` on main is "the target's
/// definition changed since the fork": MERGE refuses it (55000, naming the
/// table), whatever the branch did to `t`, instead of landing the branch's rows
/// by row id in a different table.
#[test]
fn merge_refused_when_main_recreated_a_table_since_the_fork() {
    enum Branch {
        AddColumnBefore,
        RowsOnly,
        AddColumnAfter,
    }
    for family in FAMILIES {
        for scenario in [Branch::AddColumnBefore, Branch::RowsOnly, Branch::AddColumnAfter] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
            branch(&db, "r");
            match scenario {
                Branch::AddColumnBefore => ok(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]),
                Branch::RowsOnly => ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(10), i(1)]),
                Branch::AddColumnAfter => 0,
            };
            db.execute("USE BRANCH main").unwrap();
            db.execute("DROP TABLE t").unwrap();
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
            db.execute("INSERT INTO t VALUES (100, 100)").unwrap();
            if matches!(scenario, Branch::AddColumnAfter) {
                db.execute("USE BRANCH r").unwrap();
                ok(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]);
                db.execute("USE BRANCH main").unwrap();
            }
            let rows_before = main_rows_state(&db, "t");
            let catalog_before = main_catalog_state(&db);
            let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
            assert!(
                err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER) && err.contains("t was dropped and created again"),
                "[{family:?}] {err}"
            );
            assert_eq!(main_rows_state(&db, "t"), rows_before, "[{family:?}] main's rows");
            assert_eq!(main_catalog_state(&db), catalog_before, "[{family:?}] main's catalog");
            assert!(branch_is_active(&db, "r"), "[{family:?}] the branch stays");
            assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] no intent");
        }
    }
}

/// Created on main AFTER the fork, written on the branch (B3 reads main live): no
/// fork entry to compare, so it merges. Phase B4's fork point tightens this.
#[test]
fn merge_of_rows_of_a_table_created_on_the_target_after_the_fork_is_refused() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        db.execute("CREATE BRANCH untouched AS OF NOW").unwrap();
        db.execute("CREATE TABLE later (id INT PRIMARY KEY)").unwrap();
        db.execute("USE BRANCH r").unwrap();
        ok(&db, family, "INSERT INTO later VALUES ($1)", &[i(1)]);
        db.execute("USE BRANCH main").unwrap();
        let err = db
            .execute("MERGE BRANCH r INTO main")
            .expect_err("no fork entry")
            .to_string();
        assert!(
            err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER)
                && err.contains("later was created on main after branch r forked"),
            "[{family:?}] {err}"
        );
        assert!(branch_is_active(&db, "r"), "[{family:?}]");
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM later", &[])),
            Vec::<i64>::new(),
            "[{family:?}] nothing landed"
        );
        db.execute("MERGE BRANCH untouched INTO main")
            .expect("a branch that changed no rows of it");
    }
}

// ---------------------------------------------------------------------------
// B3 fix pass, amendment B: MERGE never duplicates a key on its target
// ---------------------------------------------------------------------------

/// A merge that would duplicate a key on main is refused (23505, naming the
/// table, the constraint and the key) with nothing changed; the branch stays
/// Active, and merges once the conflicting main row is gone. Without it, the row
/// phase's claim would stop the merge half way on a key it could have refused
/// before it changed anything.
#[test]
fn a_merge_that_would_duplicate_a_key_is_refused_and_stays_mergeable() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO d VALUES (1, 'a')").unwrap();
        branch(&db, "r");
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(2), s("x")]);
        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(3), s("x")]);
        let rows_before = main_rows_state(&db, "d");
        let catalog_before = main_catalog_state(&db);

        let err = db.execute("MERGE BRANCH r INTO main").expect_err("a duplicate key");
        assert!(is_unique_violation(&err), "[{family:?}] not a 23505: {err}");
        let message = err.to_string();
        assert!(
            message.contains(BRANCH_MERGE_DUPLICATE_KEY_MARKER)
                && message.contains("table d")
                && message.contains("d_code_key")
                && message.contains("(code)=('x')"),
            "[{family:?}] {message}"
        );
        assert_eq!(main_rows_state(&db, "d"), rows_before, "[{family:?}] main's rows");
        assert_eq!(main_catalog_state(&db), catalog_before, "[{family:?}] main's catalog");
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] no intent");
        assert!(branch_is_active(&db, "r"), "[{family:?}] the branch stays Active");

        ok(&db, family, "DELETE FROM d WHERE id = $1", &[i(3)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM d ORDER BY id", &[])),
            vec![1, 2],
            "[{family:?}]"
        );
        assert_unique_violation(
            exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s("x")]),
            family,
            "main's index holds the merged key",
        );
    }
}

/// The PRIMARY KEY too; and into a branch target.
#[test]
fn a_merge_that_would_duplicate_a_primary_key_on_a_branch_is_refused() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v INT)").unwrap();
        branch(&db, "b");
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(1), i(1)]);
        db.execute("USE BRANCH b").unwrap();
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(1), i(2)]);
        db.execute("USE BRANCH main").unwrap();
        let err = db.execute("MERGE BRANCH c INTO b").expect_err("a duplicate key");
        assert!(is_unique_violation(&err), "[{family:?}] not a 23505: {err}");
        assert!(err.to_string().contains("PRIMARY KEY"), "[{family:?}] {err}");
        assert!(branch_is_active(&db, "c"), "[{family:?}]");
    }
}

/// Rows the merge itself deletes or replaces by row id do not hold their keys:
/// the branch moved key 'a' off row 1 and gave it to a new row.
#[test]
fn a_merge_that_moves_a_key_between_its_own_rows_is_not_a_conflict() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO d VALUES (1, 'a'), (2, 'b')").unwrap();
        branch(&db, "r");
        ok(&db, family, "UPDATE d SET code = $1 WHERE id = $2", &[s("z"), i(1)]);
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(3), s("a")]);
        ok(&db, family, "DELETE FROM d WHERE id = $1", &[i(2)]);
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(4), s("b")]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        let merged: Vec<(i64, Option<String>)> = rows(&db, family, "SELECT id, code FROM d ORDER BY id", &[])
            .iter()
            .map(|row| (int(row.values.first()), text(row.values.get(1))))
            .collect();
        assert_eq!(
            merged,
            vec![
                (1, Some("z".to_string())),
                (3, Some("a".to_string())),
                (4, Some("b".to_string()))
            ],
            "[{family:?}]"
        );
    }
}

// ---------------------------------------------------------------------------
// B3 fix pass: the code graph, and two sessions on the wire
// ---------------------------------------------------------------------------

/// The code-graph statements dispatch before planning; they refuse on a branch
/// first thing, so the process-wide registry stays untouched.
#[cfg(feature = "code-graph")]
#[test]
fn code_graph_ddl_is_refused_on_a_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    branch(&db, "r");
    for sql in [
        "CREATE AST INDEX idx_cap ON src (content) USING tree_sitter(lang)",
        "SELECT hdb_code.pause('idx_cap')",
        "SELECT hdb_code.resume('idx_cap')",
        "CREATE SEMANTIC HASH INDEX code_merkle",
    ] {
        let err = db.execute(sql).expect_err(sql).to_string();
        assert!(err.contains(BRANCH_SCHEMA_UNSUPPORTED_MARKER), "{sql}: {err}");
    }
}

/// A schema statement on a branch runs alone: another open transaction's staged
/// writes of the table refuse it (55006), and after they commit it succeeds with
/// their rows under the new definition. Plus the SQLSTATEs of MERGE's new
/// refusals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_column_refuses_while_another_transaction_holds_branch_writes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);
    db.execute("CREATE TABLE side (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO side VALUES (1, 1)").unwrap();
    db.execute("CREATE TABLE dup (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    let s = serve(db).await;
    let (ddl, ddl_task) = connect(s.port).await;
    let (writer, writer_task) = connect(s.port).await;
    let (on_main, main_task) = connect(s.port).await;
    ddl.simple_query("USE BRANCH r").await.unwrap();
    writer.simple_query("USE BRANCH r").await.unwrap();

    writer.simple_query("BEGIN").await.unwrap();
    writer.simple_query("INSERT INTO orders VALUES (9, 90)").await.unwrap();
    let add = "ALTER TABLE orders ADD COLUMN cur TEXT NOT NULL DEFAULT 'USD'";
    let err = ddl.simple_query(add).await.err().expect("the writer's rows are staged");
    assert_eq!(sqlstate(&err), "55006", "{err:?}");
    assert!(
        err.to_string().contains(BRANCH_TABLE_BUSY_MARKER) || format!("{err:?}").contains(BRANCH_TABLE_BUSY_MARKER)
    );
    // Another table is free.
    ddl.simple_query("ALTER TABLE dup ADD COLUMN extra INT").await.unwrap();
    writer.simple_query("COMMIT").await.unwrap();
    ddl.simple_query(add).await.unwrap();
    let row = ddl.query_one("SELECT cur FROM orders WHERE id = 9", &[]).await.unwrap();
    assert_eq!(
        row.get::<_, String>(0),
        "USD",
        "the committed row is under the definition"
    );

    // DROP, then CREATE over the name: the staged rows must not leak into it.
    writer.simple_query("BEGIN").await.unwrap();
    writer.simple_query("INSERT INTO side VALUES (5, 5)").await.unwrap();
    let err = ddl.simple_query("DROP TABLE side").await.err().expect("staged rows");
    assert_eq!(sqlstate(&err), "55006", "{err:?}");
    writer.simple_query("ROLLBACK").await.unwrap();
    ddl.simple_query("DROP TABLE side").await.unwrap();
    ddl.simple_query("CREATE TABLE side (id INT PRIMARY KEY, v INT)")
        .await
        .unwrap();
    assert!(ddl.query("SELECT id FROM side", &[]).await.unwrap().is_empty());

    // MERGE's refusals. A duplicate key: 23505.
    ddl.simple_query("INSERT INTO dup VALUES (1, 'x', 0)").await.unwrap();
    on_main.simple_query("INSERT INTO dup VALUES (2, 'x')").await.unwrap();
    // (`dup`'s definition differs: the branch added `extra`, and that is what the
    // key must be read through.)
    let err = on_main
        .simple_query("MERGE BRANCH r INTO main")
        .await
        .err()
        .expect("a duplicate key");
    assert_eq!(sqlstate(&err), "23505", "{err:?}");
    on_main.simple_query("DELETE FROM dup WHERE id = 2").await.unwrap();

    // Unfinished: 55006 for what it refuses, and the rerun finishes it.
    drop(ddl);
    drop(writer);
    ddl_task.abort();
    writer_task.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    s.db.storage.debug_fail_merge_after_steps(1);
    let err = on_main
        .simple_query("MERGE BRANCH r INTO main")
        .await
        .err()
        .expect("the injected failure");
    assert_eq!(sqlstate(&err), "55006", "{err:?}");
    let err = on_main
        .simple_query("DROP BRANCH r")
        .await
        .err()
        .expect("the source of an unfinished merge");
    assert_eq!(sqlstate(&err), "55006", "{err:?}");
    on_main.simple_query("MERGE BRANCH r INTO main").await.unwrap();
    assert_eq!(count_keys(&s.db, "branch:merge:"), 0);

    drop(on_main);
    main_task.abort();
}

// ---------------------------------------------------------------------------
// B3 second fix pass
// ---------------------------------------------------------------------------

fn session(db: &EmbeddedDatabase, name: &str) -> heliosdb_nano::session::SessionId {
    db.create_session(name, heliosdb_nano::session::IsolationLevel::ReadCommitted)
        .unwrap()
}

fn pairs(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<(i64, Option<String>)> {
    rows(db, family, sql, &[])
        .iter()
        .map(|row| (int(row.values.first()), text(row.values.get(1))))
        .collect()
}

/// `d(id PRIMARY KEY, code UNIQUE)` on main with row (1, 'a'), and branch `r`
/// that inserts (2, 'x').
fn seed_a_key_on_a_branch(db: &EmbeddedDatabase, family: Family) {
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO d VALUES (1, 'a')").unwrap();
    branch(db, "r");
    ok(db, family, "INSERT INTO d VALUES ($1, $2)", &[i(2), s("x")]);
    db.execute("USE BRANCH main").unwrap();
}

/// R5.1: what an open writer of its table holds a merge up with: it waits, for
/// `[session] lock_timeout_ms`, and says what it waited for. Small here, since nothing
/// ends the writer while the test waits.
fn short_lock_timeout(db: &EmbeddedDatabase) {
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
}

fn assert_lock_timeout(message: &str, holder: &str) {
    assert!(
        message.contains(TABLE_LOCK_TIMEOUT_MARKER) && message.contains(holder),
        "{message}"
    );
}

/// H1, B6, R5.1: an open main transaction has staged writes of a table the merge
/// changes: its keys are in main's index but not in its rows, and it may hold a row the
/// merge also writes. It holds the table's lock until it ends, so the merge waits for
/// it, whatever the keys are, as on a branch target, and gives up (55P03) with nothing
/// changed.
#[test]
fn a_merge_waits_for_a_main_transaction_with_staged_writes_on_its_table() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_a_key_on_a_branch(&db, family);
        short_lock_timeout(&db);
        let writer = session(&db, "writer");
        db.begin_transaction_for_session(writer).unwrap();
        db.execute_for_session(writer, "INSERT INTO d VALUES (3, 'x')").unwrap();

        let err = db.execute("MERGE BRANCH r INTO main").expect_err("an open writer");
        let message = err.to_string();
        assert_lock_timeout(&message, "an open transaction holds it");
        assert!(message.contains(" d:"), "[{family:?}] {message}");
        assert!(branch_is_active(&db, "r"), "[{family:?}] nothing changed");
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] no intent");

        // It holds the key for real once it commits: now a 23505.
        db.commit_transaction_for_session(writer).unwrap();
        let err = db
            .execute("MERGE BRANCH r INTO main")
            .expect_err("the key is committed");
        assert!(is_unique_violation(&err), "[{family:?}] {err}");
        ok(&db, family, "DELETE FROM d WHERE id = $1", &[i(3)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM d ORDER BY id", &[])), vec![1, 2]);
    }
}

/// B6: the review's false pass. An open transaction moves row 1 to the key the
/// merge's new row takes, and the merge also rewrites row 1: both were "the rows it
/// replaces" to validation, and the COMMIT then duplicated the key. R5.1: the merge
/// waits for that transaction.
#[test]
fn a_merge_waits_for_an_open_transaction_that_moved_a_row_it_also_writes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    short_lock_timeout(&db);
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO d VALUES (1, 'a')").unwrap();
    branch(&db, "r");
    db.execute("UPDATE d SET code = 'z' WHERE id = 1").unwrap();
    db.execute("INSERT INTO d VALUES (2, 'x')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let writer = session(&db, "writer");
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE d SET code = 'x' WHERE id = 1")
        .unwrap();
    let err = db
        .execute("MERGE BRANCH r INTO main")
        .expect_err("an open writer")
        .to_string();
    assert_lock_timeout(&err, "an open transaction holds it");
    db.rollback_transaction_for_session(writer).unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(
        pairs(&db, Family::Text, "SELECT id, code FROM d ORDER BY id"),
        vec![(1, Some("z".to_string())), (2, Some("x".to_string()))]
    );
}

/// A transaction that rolls back gives the key up, and the merge goes through.
#[test]
fn a_merge_goes_through_once_the_claiming_transaction_rolled_back() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    short_lock_timeout(&db);
    seed_a_key_on_a_branch(&db, Family::Text);
    let writer = session(&db, "writer");
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "INSERT INTO d VALUES (3, 'x')").unwrap();
    assert!(db.execute("MERGE BRANCH r INTO main").is_err());
    db.rollback_transaction_for_session(writer).unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM d ORDER BY id", &[])),
        vec![1, 2]
    );
}

/// A pending merge leaves main's DML open, so a resume validates its keys again.
#[test]
fn a_resumed_merge_validates_its_keys_again() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_a_key_on_a_branch(&db, family);
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");

        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(3), s("x")]);
        let err = db.execute("MERGE BRANCH r INTO main").expect_err("a duplicate key");
        assert!(is_unique_violation(&err), "[{family:?}] {err}");
        assert_eq!(count_keys(&db, "branch:merge:"), 1, "[{family:?}] still pending");

        ok(&db, family, "DELETE FROM d WHERE id = $1", &[i(3)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] finished");
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM d ORDER BY id", &[])), vec![1, 2]);
    }
}

/// H2: rows are copied by position, so a rows-only merge across a definition
/// change on main is refused (55000), naming the table and what changed. It used
/// to land `(1,10,21)` under a two-column schema, and main then read `b = 10`.
#[test]
fn a_rows_only_merge_is_refused_when_mains_definition_changed_since_the_fork() {
    for family in FAMILIES {
        for (change, what) in [
            ("ALTER TABLE t DROP COLUMN a", "are now (id, b)"),
            ("ALTER TABLE t ADD COLUMN c INT", "are now (id, a, b, c)"),
            (
                "ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 0",
                "are now (id, a, b, c)",
            ),
            (
                "CREATE UNIQUE INDEX t_a_key ON t (a)",
                "its constraints or unique indexes",
            ),
        ] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 10, 20)").unwrap();
            branch(&db, "r");
            ok(&db, family, "UPDATE t SET b = $1 WHERE id = $2", &[i(21), i(1)]);
            db.execute("USE BRANCH main").unwrap();
            db.execute(change).unwrap();
            let rows_before = main_rows_state(&db, "t");
            let catalog_before = main_catalog_state(&db);

            let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
            assert!(
                err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER) && err.contains("t (") && err.contains(what),
                "[{family:?}] {change}: {err}"
            );
            assert_eq!(
                main_rows_state(&db, "t"),
                rows_before,
                "[{family:?}] {change}: main's rows"
            );
            assert_eq!(
                main_catalog_state(&db),
                catalog_before,
                "[{family:?}] {change}: main's catalog"
            );
            assert!(branch_is_active(&db, "r"), "[{family:?}] {change}");
            assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] {change}");
        }
    }
}

/// A table renamed away on main is not "dropped and created again".
#[test]
fn a_merge_of_a_table_main_renamed_says_so() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    branch(&db, "r");
    db.execute("UPDATE t SET v = 2 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.execute("ALTER TABLE t RENAME TO renamed").unwrap();
    let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
    assert!(
        err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER)
            && err.contains("t no longer exists")
            && !err.contains("created again"),
        "{err}"
    );
}

/// c → b is stopped part way. b cannot be merged away from under it, main's DDL on
/// what the merge inherits from main is refused, and finishing c → b frees both.
#[test]
fn a_merge_is_refused_when_it_would_strand_a_pending_one() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
        branch(&db, "b");
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(2), i(1)]);
        db.execute("USE BRANCH main").unwrap();
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        let err = db.execute("MERGE BRANCH c INTO b").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");

        let err = db.execute("MERGE BRANCH b INTO main").expect_err("b is its target");
        assert!(
            err.to_string().contains(BRANCH_MERGE_PENDING_MARKER),
            "[{family:?}] {err}"
        );
        assert!(branch_is_active(&db, "b"), "[{family:?}]");

        let err = exec(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]).expect_err("b inherits t");
        assert!(
            err.to_string().contains(BRANCH_MERGE_PENDING_MARKER),
            "[{family:?}] {err}"
        );
        let err = db.execute("DROP TABLE t").expect_err("b inherits t");
        assert!(
            err.to_string().contains(BRANCH_MERGE_PENDING_MARKER),
            "[{family:?}] {err}"
        );

        db.execute("MERGE BRANCH c INTO b").unwrap();
        db.execute("MERGE BRANCH b INTO main").unwrap();
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]);
        let merged = rows(&db, family, "SELECT v FROM t", &[]);
        assert_eq!(int(merged[0].values.first()), 2, "[{family:?}]");
    }
}

/// Two merges cannot hold one table of one level; the second is told to wait, not
/// to "run it again" (it has nothing to finish).
#[test]
fn a_second_merge_of_a_held_table_is_told_to_wait() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
    db.execute("CREATE BRANCH one AS OF NOW").unwrap();
    db.execute("CREATE BRANCH two AS OF NOW").unwrap();
    db.execute("USE BRANCH one").unwrap();
    db.execute("UPDATE t SET v = 10 WHERE id = 1").unwrap();
    db.execute("USE BRANCH two").unwrap();
    db.execute("UPDATE t SET v = 20 WHERE id = 2").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
    db.execute("MERGE BRANCH one INTO main").unwrap_err();

    let err = db.execute("MERGE BRANCH two INTO main").unwrap_err().to_string();
    assert!(
        err.contains(BRANCH_MERGE_BUSY_MARKER) && !err.contains(BRANCH_MERGE_PENDING_MARKER),
        "{err}"
    );
    assert!(branch_is_active(&db, "two"), "it changed nothing");
    db.execute("MERGE BRANCH one INTO main").unwrap();
    db.execute("MERGE BRANCH two INTO main").unwrap();
}

/// The way out. `MERGE BRANCH x ABORT` returns the source to Active, lets go of
/// everything the merge held, and says which tables may be partially changed.
#[test]
fn abort_abandons_a_pending_merge_and_names_its_tables() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_merge_of_three_steps(&db, family);
        db.storage.debug_fail_merge_after_steps(1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
        assert!(!branch_is_active(&db, "r"), "[{family:?}] the source is merged");

        let tables: Vec<Option<String>> = rows(&db, family, "MERGE BRANCH r ABORT", &[])
            .iter()
            .map(|row| text(row.values.first()))
            .collect();
        assert_eq!(
            tables,
            vec![
                Some("a_new".to_string()),
                Some("b_added".to_string()),
                Some("c_gone".to_string())
            ],
            "[{family:?}] what it may have partially changed"
        );
        assert!(branch_is_active(&db, "r"), "[{family:?}] the source is Active again");
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}] no intent");
        // What ran stays: nothing keeps a pre-image to undo it with. What it held
        // is free.
        ok(&db, family, "ALTER TABLE b_added ADD COLUMN q INT", &[]);
        ok(&db, family, "DROP TABLE c_gone", &[]);
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM a_new", &[])), Vec::<i64>::new());

        // There is nothing left to abort, and the branch can go.
        let err = try_rows(&db, family, "MERGE BRANCH r ABORT", &[]).expect_err("no merge");
        assert!(err.to_string().contains("no unfinished merge"), "[{family:?}] {err}");
        db.execute("DROP BRANCH r").unwrap();
    }
}

/// `MERGE BRANCH main INTO x` would mark main Merged; a branch into itself would
/// wait for itself.
#[test]
fn merging_main_or_a_branch_into_itself_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    let err = db.execute("MERGE BRANCH main INTO b").unwrap_err().to_string();
    assert!(err.contains("main is not a branch that can be merged"), "{err}");
    let err = db.execute("MERGE BRANCH b INTO b").unwrap_err().to_string();
    assert!(err.contains("into itself"), "{err}");
    assert!(branch_is_active(&db, "main"));
}

/// M1: a row merged into a branch is in the level's key index the moment it lands,
/// including one no materialization ever saw.
#[test]
fn a_merged_row_is_in_the_targets_key_index() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        branch(&db, "b");
        ok(
            &db,
            family,
            "CREATE TABLE k (id INT PRIMARY KEY, code TEXT UNIQUE)",
            &[],
        );
        ok(&db, family, "INSERT INTO k VALUES ($1, $2)", &[i(1), s("one")]);
        ok(&db, family, "INSERT INTO k VALUES ($1, $2)", &[i(2), s("two")]);
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        ok(&db, family, "INSERT INTO k VALUES ($1, $2)", &[i(5), s("five")]);
        ok(&db, family, "UPDATE k SET code = $1 WHERE id = $2", &[s("uno"), i(1)]);
        ok(&db, family, "DELETE FROM k WHERE id = $1", &[i(2)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH c INTO b").unwrap();

        db.execute("USE BRANCH b").unwrap();
        let ins = "INSERT INTO k VALUES ($1, $2)";
        assert_unique_violation(exec(&db, family, ins, &[i(5), s("x")]), family, "the merged PK");
        assert_unique_violation(exec(&db, family, ins, &[i(9), s("five")]), family, "the merged UNIQUE");
        assert_unique_violation(exec(&db, family, ins, &[i(9), s("uno")]), family, "the merged update");
        ok(&db, family, ins, &[i(6), s("one")]);
        ok(&db, family, ins, &[i(2), s("two")]);
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM k ORDER BY id", &[])),
            vec![1, 2, 5, 6],
            "[{family:?}]"
        );
    }
}

/// MERGE c INTO b, stopped before and after the record lands, twice over: the rerun
/// applies the step once (the record is computed at plan time and written once).
#[test]
fn put_on_branch_reruns_are_idempotent() {
    // A chunk of one row, so a two-row materialization reaches a boundary. Harmless
    // to whatever else runs in this process: it only writes smaller batches.
    EmbeddedDatabase::new_in_memory()
        .unwrap()
        .storage
        .debug_set_materialize_chunk_rows(1);
    for family in FAMILIES {
        for site in [MergeFailSite::Step, MergeFailSite::Materialize, MergeFailSite::Row] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            seed_put_on_branch(&db, family);

            db.storage.debug_fail_merge_at(site, 0);
            let first = db.execute("MERGE BRANCH c INTO b");
            assert!(
                first.unwrap_err().to_string().contains(BRANCH_MERGE_PENDING_MARKER),
                "[{family:?}] {site:?}"
            );
            db.execute("MERGE BRANCH c INTO b").unwrap();
            assert_put_on_branch_finished(&db, family);

            db.execute("USE BRANCH main").unwrap();
            db.execute("MERGE BRANCH b INTO main").unwrap();
            assert_eq!(
                listed_columns(&db, family, "t"),
                vec!["id", "v", "cur"],
                "[{family:?}] {site:?}"
            );
        }
    }
}

/// `t` (ids 1, 2) on branch `b`, and its child `c`, which adds a column and a row.
fn seed_put_on_branch(db: &EmbeddedDatabase, family: Family) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
    branch(db, "b");
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    db.execute("USE BRANCH c").unwrap();
    ok(db, family, "ALTER TABLE t ADD COLUMN cur TEXT DEFAULT 'USD'", &[]);
    ok(
        db,
        family,
        "INSERT INTO t VALUES ($1, $2, $3)",
        &[i(20), i(20), s("NEW")],
    );
    db.execute("USE BRANCH main").unwrap();
}

/// Reads `b` as `MERGE BRANCH c INTO b` left it, and leaves the session on `b`.
fn assert_put_on_branch_finished(db: &EmbeddedDatabase, family: Family) {
    db.execute("USE BRANCH b").unwrap();
    assert_eq!(listed_columns(db, family, "t"), vec!["id", "v", "cur"], "[{family:?}]");
    assert_eq!(
        ids(&rows(db, family, "SELECT id FROM t ORDER BY id", &[])),
        vec![1, 2, 20],
        "[{family:?}]"
    );
    // A key no materialization indexed: only the row phase's own entry can refuse
    // this.
    assert_unique_violation(
        exec(db, family, "INSERT INTO t VALUES ($1, $2, $3)", &[i(20), i(0), s("x")]),
        family,
        "a key that arrived in the row phase",
    );
}

/// A failure in the row phase, between two tables. The first has landed whole, and
/// main's indexes agree with its rows at once, without waiting for the rerun. The
/// second has not landed at all. The rerun finishes.
#[test]
fn a_stopped_row_phase_leaves_mains_indexes_right() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE e (id INT PRIMARY KEY)").unwrap();
        seed_two_keys_on_a_branch(&db, family);
        db.execute("USE BRANCH r").unwrap();
        ok(&db, family, "INSERT INTO e VALUES ($1)", &[i(1)]);
        db.execute("USE BRANCH main").unwrap();
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 1);
        let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "[{family:?}] {err}");

        assert_eq!(
            pairs(&db, family, "SELECT id, code FROM d ORDER BY id"),
            some_pairs(&[(1, "a"), (2, "x"), (3, "y")]),
            "[{family:?}] all of `d`"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            Vec::<i64>::new(),
            "[{family:?}] none of `e`"
        );
        for code in ["x", "y"] {
            assert_unique_violation(
                exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(7), s(code)]),
                family,
                "a row the stopped merge landed",
            );
        }
        assert_eq!(dense_count(&db), 3, "[{family:?}]");

        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            vec![1],
            "[{family:?}]"
        );
        assert_eq!(dense_count(&db), 3, "[{family:?}]");
        assert_eq!(count_keys(&db, "branch:merge:"), 0);
    }
}

/// A real crash: a child process runs the merge up to the failure and exits with
/// no destructor running. The next open finishes it.
#[test]
fn merge_interrupted_by_a_crash_is_finished_at_the_next_open() {
    const CHILD: &str = "HELIOSDB_B3_MERGE_CRASH_DIR";
    if let Ok(path) = std::env::var(CHILD) {
        let db = EmbeddedDatabase::new(&path).unwrap();
        seed_merge_of_three_steps(&db, Family::Text);
        db.storage.debug_fail_merge_after_steps(1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("merge_interrupted_by_a_crash_is_finished_at_the_next_open")
        .env(CHILD, dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "the child: {status:?}");
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_merge_of_three_steps_finished(&db, Family::Text);
}

/// An intent on a store opened read-only, and on a fenced primary: nothing is
/// finished, and nothing is torn down trying. The next writable open finishes it.
#[test]
fn an_open_that_cannot_write_leaves_an_intent_alone() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        seed_merge_of_three_steps(&db, Family::Text);
        db.storage.debug_fail_merge_after_steps(1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
    }
    {
        let db = EmbeddedDatabase::open_read_only(dir.path()).unwrap();
        assert_eq!(count_keys(&db, "branch:merge:"), 1, "the intent is still there");
        assert!(db.storage.finish_pending_branch_merges().is_empty());
        assert_eq!(
            ids(&rows(&db, Family::Text, "SELECT id FROM c_gone", &[])),
            vec![1],
            "the step that had not run still has not"
        );
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_merge_of_three_steps_finished(&db, Family::Text);

    // Fenced: the same, on a handle whose gate refuses every write. The merge's
    // drop must not reach main's in-memory indexes before the gate says no.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_merge_of_three_steps(&db, Family::Text);
    db.storage.debug_fail_merge_after_steps(1);
    db.execute("MERGE BRANCH r INTO main").unwrap_err();
    let _fenced = db.storage.write_gate().fence("fenced by the test");
    assert!(db.storage.finish_pending_branch_merges().is_empty());
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM c_gone WHERE id = 1", &[])),
        vec![1]
    );
    assert_eq!(count_keys(&db, "branch:merge:"), 1);
}

/// M2: main's refusal is in the catalog funnels, so routes that plan no statement
/// through either executor family are covered too.
#[test]
fn every_route_to_mains_catalog_refuses_a_table_a_pending_merge_holds() {
    let mut db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SCHEMA s").unwrap();
    db.execute("CREATE TABLE s.held (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO s.held VALUES (1, 1)").unwrap();
    seed_merge_of_three_steps(&db, Family::Text);
    db.execute("USE BRANCH r").unwrap();
    db.execute("UPDATE s.held SET v = 2 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_after_steps(1);
    db.execute("MERGE BRANCH r INTO main").unwrap_err();
    let pending = |result: heliosdb_nano::Result<()>, what: &str| {
        let message = result.expect_err(what).to_string();
        assert!(message.contains(BRANCH_MERGE_PENDING_MARKER), "{what}: {message}");
    };
    pending(db.query("DROP TABLE c_gone", &[]).map(drop), "db.query");
    pending(db.execute("DROP SCHEMA s CASCADE").map(drop), "DROP SCHEMA … CASCADE");
    pending(db.storage.catalog().drop_table("c_gone"), "the DROP funnel");
    pending(
        db.storage.catalog().rename_table("c_gone", "elsewhere"),
        "the RENAME funnel",
    );
    pending(
        db.storage
            .catalog()
            .create_table("c_gone", heliosdb_nano::Schema::new(Vec::new())),
        "the CREATE funnel",
    );
    pending(
        db.storage
            .catalog()
            .save_table_constraints("c_gone", &heliosdb_nano::sql::TableConstraints::default()),
        "the constraint funnel",
    );
    assert!(
        db.query("SELECT id FROM s.held", &[]).is_ok_and(|rows| rows.len() == 1),
        "s.held and its schema are still there"
    );
    let refused = db
        .restore_from_dump(std::path::Path::new("/nonexistent"))
        .expect_err("a restore")
        .to_string();
    assert!(refused.contains(BRANCH_MERGE_PENDING_MARKER), "{refused}");
}

/// `db.query` runs a schema statement on a branch like every other entry point: it
/// used to reach the executor's backstop and get a wrong 0A000.
#[test]
fn a_schema_statement_through_query_applies_to_the_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE seed (id INT PRIMARY KEY)").unwrap();
    branch(&db, "r");
    assert!(db
        .query("CREATE TABLE fresh (id INT PRIMARY KEY)", &[])
        .unwrap()
        .is_empty());
    db.execute("INSERT INTO fresh VALUES (1)").unwrap();
    assert_eq!(ids(&rows(&db, Family::Text, "SELECT id FROM fresh", &[])), vec![1]);
    db.query("DROP TABLE seed", &[]).unwrap();
    assert_missing_table(
        try_rows(&db, Family::Text, "SELECT * FROM seed", &[]),
        Family::Text,
        "dropped",
    );
    db.execute("USE BRANCH main").unwrap();
    assert_missing_table(
        try_rows(&db, Family::Text, "SELECT * FROM fresh", &[]),
        Family::Text,
        "main",
    );
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM seed", &[])),
        Vec::<i64>::new()
    );
}

/// M3: staged writes on the source would land after the keys were validated.
#[test]
fn a_merge_waits_for_a_transaction_with_staged_writes_on_the_source() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    short_lock_timeout(&db);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    let writer = session(&db, "writer");
    db.execute_for_session(writer, "USE BRANCH r").unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "INSERT INTO t VALUES (1, 1)").unwrap();

    let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
    assert_lock_timeout(&err, "an open transaction holds it");
    assert!(branch_is_active(&db, "r"));
    db.commit_transaction_for_session(writer).unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(ids(&rows(&db, Family::Text, "SELECT id FROM t", &[])), vec![1]);
}

/// Into a branch, an open transaction on the TARGET holds the merge up too: its
/// staged keys were checked against rows the merge is about to change.
#[test]
fn a_merge_waits_for_a_transaction_with_staged_writes_on_the_target() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    short_lock_timeout(&db);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    branch(&db, "b");
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    db.execute("USE BRANCH c").unwrap();
    db.execute("INSERT INTO t VALUES (5, 5)").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let writer = session(&db, "writer");
    db.execute_for_session(writer, "USE BRANCH b").unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "INSERT INTO t VALUES (6, 6)").unwrap();
    let err = db.execute("MERGE BRANCH c INTO b").unwrap_err().to_string();
    assert_lock_timeout(&err, "an open transaction holds it");
    db.commit_transaction_for_session(writer).unwrap();
    db.execute("MERGE BRANCH c INTO b").unwrap();
}

/// M4: a merged child stays in its own children's chains: b → c → d, c merged into
/// b. b's DDL would change what d reads.
#[test]
fn ddl_is_refused_while_a_live_branch_reads_through_a_merged_child() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        branch(&db, "b");
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH c").unwrap();
        db.execute("CREATE BRANCH d AS OF NOW").unwrap();
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH c INTO b").unwrap();
        db.execute("USE BRANCH b").unwrap();
        for sql in [
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN x INT",
            "CREATE TABLE fresh (id INT)",
        ] {
            let err = exec(&db, family, sql, &[]).expect_err(sql).to_string();
            assert!(
                err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER) && err.contains("children: d"),
                "[{family:?}] {sql}: {err}"
            );
        }
        db.execute("USE BRANCH main").unwrap();
        db.execute("DROP BRANCH d").unwrap();
        db.execute("USE BRANCH b").unwrap();
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT", &[]);
    }
}

/// M5: b's schema change is on main once b merged, so c, forked from b, has nothing
/// left to wait for. It used to be refused for good: b could not be merged again,
/// and c could not merge into b, which was no longer active.
#[test]
fn a_child_merges_after_its_parent_merged_into_the_same_target() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
        branch(&db, "b");
        ok(&db, family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
        db.execute("CREATE BRANCH c AS OF NOW").unwrap();
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH b INTO main").unwrap();

        db.execute("USE BRANCH c").unwrap();
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(10), i(1)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH c INTO main").unwrap();
        let merged: Vec<(i64, i64)> = rows(&db, family, "SELECT id, v, x FROM t ORDER BY id", &[])
            .iter()
            .map(|row| (int(row.values.get(1)), int(row.values.get(2))))
            .collect();
        assert_eq!(merged, vec![(10, 7), (2, 7)], "[{family:?}]");
    }
}

/// A table with no incarnation yet (a backfill that failed) is unknown, not
/// different: the backfill that later gives it an id refuses nothing.
#[test]
fn a_table_with_no_incarnation_compares_as_unknown() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE legacy (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO legacy VALUES (1, 1)").unwrap();
        db.storage.delete(&b"meta:table_incarnation:legacy".to_vec()).unwrap();
        branch(&db, "r");
        db.execute("ALTER TABLE legacy ADD COLUMN x INT DEFAULT 5").unwrap();
        db.execute("UPDATE legacy SET v = 2 WHERE id = 1").unwrap();
        db.execute("USE BRANCH main").unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(raw_key(&db, "meta:table_incarnation:legacy").is_some(), "backfilled");
    db.execute("MERGE BRANCH r INTO main").unwrap();
    let merged = rows(&db, Family::Text, "SELECT v, x FROM legacy", &[]);
    assert_eq!(int(merged[0].values.first()), 2);
    assert_eq!(int(merged[0].values.get(1)), 5);
}

/// A MERGE's drop is main's drop: partition children go with their parent, on the
/// branch and on main, and a dropped table's triggers leave the live registry.
#[test]
fn a_merged_drop_cascades_to_partitions_and_deregisters_triggers() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")
            .unwrap();
        db.execute("CREATE TABLE child_low PARTITION OF parent FOR VALUES FROM (0) TO (100)")
            .unwrap();
        db.execute("CREATE TABLE doomed (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$")
            .unwrap();
        db.execute("CREATE TRIGGER trg BEFORE INSERT ON doomed FOR EACH ROW EXECUTE FUNCTION f()")
            .unwrap();
        assert!(db.trigger_registry.has_triggers_for_table("doomed"));
        branch(&db, "r");
        ok(&db, family, "DROP TABLE parent", &[]);
        ok(&db, family, "DROP TABLE doomed", &[]);
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM child_low", &[]),
            family,
            "the child goes with its parent on the branch",
        );
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM child_low", &[])),
            Vec::<i64>::new(),
            "[{family:?}] main is untouched"
        );
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_missing_table(try_rows(&db, family, "SELECT * FROM parent", &[]), family, "merged");
        assert_missing_table(
            try_rows(&db, family, "SELECT * FROM child_low", &[]),
            family,
            "cascaded",
        );
        assert!(
            !db.trigger_registry.has_triggers_for_table("doomed"),
            "[{family:?}] the live registry"
        );
    }
}

/// Composite keys are compared whole, and NULLs never collide.
#[test]
fn merge_key_validation_handles_composite_keys_and_nulls() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE c (a INT, b INT, note TEXT UNIQUE, PRIMARY KEY (a, b))")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1, 1, NULL)").unwrap();
        branch(&db, "r");
        let ins = "INSERT INTO c VALUES ($1, $2, $3)";
        ok(&db, family, ins, &[i(1), i(2), Value::Null]);
        db.execute("USE BRANCH main").unwrap();
        // The same first column, and a NULL UNIQUE value like the branch's.
        ok(&db, family, ins, &[i(1), i(3), Value::Null]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(rows(&db, family, "SELECT a FROM c", &[]).len(), 3, "[{family:?}]");

        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE c (a INT, b INT, PRIMARY KEY (a, b))").unwrap();
        branch(&db, "r");
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(1), i(2)]);
        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(1), i(2)]);
        let err = db.execute("MERGE BRANCH r INTO main").expect_err("the whole key");
        assert!(is_unique_violation(&err), "[{family:?}] {err}");
        assert!(err.to_string().contains("(a, b)=(1, 2)"), "[{family:?}] {err}");
    }
}

/// Two merged rows with one key: a table the branch created has no target rows to
/// probe, so this is the only check there is. (Written below the branch's own
/// enforcement: SQL could not put them there.)
#[test]
fn two_merged_rows_with_one_key_are_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    branch(&db, "r");
    db.execute("CREATE TABLE made (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let level = db.storage.get_branch("r").unwrap().branch_id;
    let view = heliosdb_nano::storage::BranchView::for_chain(&db.storage, "r", vec![level]).unwrap();
    // Rows no statement could write, which is the point: below every statement, so
    // with no writer lock, and nothing runs beside them.
    db.storage.debug_plant_rows(|| {
        view.put(None, "made", 1, &Tuple::new(vec![i(7), i(1)])).unwrap();
        view.put(None, "made", 2, &Tuple::new(vec![i(7), i(2)])).unwrap();
    });

    let err = db.execute("MERGE BRANCH r INTO main").expect_err("one key twice");
    assert!(is_unique_violation(&err), "{err}");
    assert!(err.to_string().contains("(id)=(7)"), "{err}");
    assert!(branch_is_active(&db, "r"));
    assert_missing_table(
        try_rows(&db, Family::Text, "SELECT * FROM made", &[]),
        Family::Text,
        "nothing merged",
    );
}

/// `MERGE BRANCH x ABORT` over the wire: its rows in the simple protocol, and it
/// parses in the extended one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_over_the_wire() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    db.execute("USE BRANCH r").unwrap();
    db.execute("UPDATE t SET v = 2 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
    db.execute("MERGE BRANCH r INTO main").unwrap_err();
    let s = serve(db).await;
    let (client, task) = connect(s.port).await;

    let answer = client.simple_query("MERGE BRANCH r ABORT").await.unwrap();
    let named: Vec<String> = answer
        .iter()
        .filter_map(|message| match message {
            tokio_postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .collect();
    assert_eq!(named, vec!["t".to_string()]);
    assert_eq!(count_keys(&s.db, "branch:merge:"), 0);

    let err = client
        .simple_query("MERGE BRANCH r ABORT")
        .await
        .err()
        .expect("nothing to abort");
    assert!(err.as_db_error().is_some(), "{err:?}");

    // The extended protocol, from the Parse to a real abort.
    s.db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
    s.db.execute("MERGE BRANCH r INTO main").unwrap_err();
    assert_eq!(count_keys(&s.db, "branch:merge:"), 1);
    client.execute("MERGE BRANCH r ABORT", &[]).await.unwrap();
    assert_eq!(count_keys(&s.db, "branch:merge:"), 0);
    assert!(branch_is_active(&s.db, "r"));

    let err = client
        .execute("MERGE BRANCH r ABORT", &[])
        .await
        .err()
        .expect("the extended protocol parses it, and there is nothing to abort");
    assert!(format!("{err:?}").contains("no unfinished merge"), "{err:?}");

    drop(client);
    task.abort();
}

// ---------------------------------------------------------------------------
// B3 third fix pass: main's indexes follow each row; a statement the merge waits
// for completes; every stage of a merge survives a crash
// ---------------------------------------------------------------------------

/// Stops whoever reaches the `nth` tick of `site` there, with no lock held. The first
/// channel says it got there; dropping the second lets it go on (also when the test
/// panics). No sleep: the two sides hand over through channels.
fn stop_at_site(db: &EmbeddedDatabase, site: MergeFailSite, nth: usize) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let ticks = AtomicUsize::new(0);
    db.storage.debug_on_merge_site(
        site,
        Some(Arc::new(move || {
            if ticks.fetch_add(1, Ordering::SeqCst) == nth {
                let _ = reached_tx.lock().unwrap().send(());
                let _ = go_rx.lock().unwrap().recv();
            }
        })),
    );
    (reached_rx, go_tx)
}

/// Runs `merge` on a thread of its own and stops it at the `nth` tick of `site`.
/// `during` runs while it is stopped there, then it goes on.
fn during_a_merge<T>(
    db: &EmbeddedDatabase,
    merge: &str,
    site: MergeFailSite,
    nth: usize,
    during: impl FnOnce() -> T,
) -> (heliosdb_nano::Result<u64>, T) {
    let (reached_rx, go_tx) = stop_at_site(db, site, nth);
    let outcome = std::thread::scope(|scope| {
        let merging = scope.spawn(|| db.execute(merge));
        while reached_rx.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!merging.is_finished(), "the merge ended before tick {nth} of {site:?}");
        }
        let value = during();
        drop(go_tx);
        (merging.join().unwrap(), value)
    });
    db.storage.debug_on_merge_site(site, None);
    outcome
}

/// R5.1. Runs `merge` on a thread of its own and stops it at the first tick of `site`,
/// holding its tables. `writer` starts on a thread of its own and is seen to wait for
/// `table`; `during` runs while it does; then the merge goes on, and the writer after
/// it. No sleep, no race: the writer cannot finish while the merge is stopped.
fn a_writer_waiting_for_a_merge<T: Send>(
    db: &EmbeddedDatabase,
    merge: &str,
    site: MergeFailSite,
    table: &str,
    writer: impl FnOnce() -> T + Send,
    during: impl FnOnce(),
) -> (heliosdb_nano::Result<u64>, T) {
    let (reached_rx, go_tx) = stop_at_site(db, site, 0);
    let outcome = std::thread::scope(|scope| {
        let merging = scope.spawn(|| db.execute(merge));
        while reached_rx.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!merging.is_finished(), "the merge ended before {site:?}");
        }
        let writing = scope.spawn(writer);
        wait_for("the writer to wait for the table", || {
            db.storage.debug_table_lock_waiters(table).1 > 0
        });
        assert!(!writing.is_finished(), "the writer waits for the merge");
        during();
        drop(go_tx);
        let merged = merging.join().unwrap();
        (merged, writing.join().unwrap())
    });
    db.storage.debug_on_merge_site(site, None);
    outcome
}

/// `statement` runs and stops past its guard, where it is counted and checked;
/// `during` runs while it is stopped there, then it goes on.
fn during_a_schema_statement<T>(
    db: &EmbeddedDatabase,
    statement: &str,
    during: impl FnOnce() -> T,
) -> (heliosdb_nano::Result<u64>, T) {
    let (reached_rx, go_tx) = stop_at_site(db, MergeFailSite::MainDdlStatement, 0);
    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| db.execute(statement));
        while reached_rx.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!running.is_finished(), "the statement ended before its guard");
        }
        let value = during();
        drop(go_tx);
        (running.join().unwrap(), value)
    });
    db.storage.debug_on_merge_site(MergeFailSite::MainDdlStatement, None);
    outcome
}

/// `statement` runs and stops past its guard; `merge` starts, registers its
/// exclusion of `table`, and is seen to wait; then the statement goes on. Returns
/// how each ended.
fn a_merge_registering_during_a_schema_statement(
    db: &EmbeddedDatabase,
    statement: &str,
    merge: &str,
    table: &str,
) -> (heliosdb_nano::Result<u64>, heliosdb_nano::Result<u64>) {
    let (reached_rx, go_tx) = stop_at_site(db, MergeFailSite::MainDdlStatement, 0);
    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| db.execute(statement));
        while reached_rx.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!running.is_finished(), "the statement ended before its guard");
        }
        let merging = scope.spawn(|| db.execute(merge));
        wait_for("the merge to hold the table", || {
            db.storage.debug_merge_holds_table(table)
        });
        // It cannot finish while the statement is counted, so this is no race.
        assert!(!merging.is_finished(), "the merge waits for the statement");
        drop(go_tx);
        (running.join().unwrap(), merging.join().unwrap())
    });
    db.storage.debug_on_merge_site(MergeFailSite::MainDdlStatement, None);
    outcome
}

/// A condition another thread will make true: wait on its state, never on time.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// `d(id PRIMARY KEY, code UNIQUE)` on main with (1, 'a'), and branch `r` that
/// inserts (2, 'x') and (3, 'y').
fn seed_two_keys_on_a_branch(db: &EmbeddedDatabase, family: Family) {
    seed_a_key_on_a_branch(db, family);
    db.execute("USE BRANCH r").unwrap();
    ok(db, family, "INSERT INTO d VALUES ($1, $2)", &[i(3), s("y")]);
    db.execute("USE BRANCH main").unwrap();
}

/// `COUNT(*)` through the dense-int statistics of `d`'s PRIMARY KEY.
fn dense_count(db: &EmbeddedDatabase) -> i64 {
    int(rows(db, Family::Text, "SELECT COUNT(*) FROM d WHERE id >= 1", &[])[0]
        .values
        .first())
}

fn some_pairs(expected: &[(i64, &str)]) -> Vec<(i64, Option<String>)> {
    expected
        .iter()
        .map(|(id, code)| (*id, Some((*code).to_string())))
        .collect()
}

/// R5.1. A transaction that opens while a merge is landing a table and moves and adds
/// keys of OTHER rows waits for the merge, and then nothing the merge did disturbs its
/// entries, whether it commits or rolls back. Before the lock it ran beside the merge,
/// and the re-derivation of the keys dropped its entries, put dead ones back, and left
/// the dense-int statistics behind the rows.
#[test]
fn a_transaction_during_a_merge_waits_and_keeps_its_index_work() {
    for commit in [true, false] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_two_keys_on_a_branch(&db, Family::Text);
        let writer = session(&db, "writer");
        let (merged, wrote) = a_writer_waiting_for_a_merge(
            &db,
            "MERGE BRANCH r INTO main",
            MergeFailSite::TableClaimed,
            "d",
            || {
                db.begin_transaction_for_session(writer)?;
                db.execute_for_session(writer, "UPDATE d SET code = 'b' WHERE id = 1")?;
                db.execute_for_session(writer, "INSERT INTO d VALUES (4, 'q')")
            },
            || {},
        );
        merged.unwrap();
        wrote.unwrap();
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "commit: {commit}");

        for merged_key in ["x", "y"] {
            assert_unique_violation(
                db.execute(&format!("INSERT INTO d VALUES (9, '{merged_key}')")),
                Family::Text,
                "a merged key",
            );
        }
        if commit {
            db.commit_transaction_for_session(writer).unwrap();
            assert_eq!(dense_count(&db), 4, "the rows the dense-int statistics count");
            assert_eq!(
                ids(&rows(&db, Family::Text, "SELECT id FROM d WHERE id = 4", &[])),
                vec![4],
                "its row is in the PRIMARY KEY index"
            );
            for held in ["q", "b"] {
                assert_unique_violation(
                    db.execute(&format!("INSERT INTO d VALUES (9, '{held}')")),
                    Family::Text,
                    "its keys",
                );
            }
            db.execute("INSERT INTO d VALUES (5, 'a')")
                .expect("the key it moved away from is free");
            assert_eq!(
                pairs(&db, Family::Text, "SELECT id, code FROM d ORDER BY id"),
                some_pairs(&[(1, "b"), (2, "x"), (3, "y"), (4, "q"), (5, "a")])
            );
        } else {
            db.rollback_transaction_for_session(writer).unwrap();
            assert_eq!(dense_count(&db), 3, "the rows the dense-int statistics count");
            assert_unique_violation(
                db.execute("INSERT INTO d VALUES (9, 'a')"),
                Family::Text,
                "row 1 holds its key again",
            );
            db.execute("INSERT INTO d VALUES (5, 'q')").expect("given up");
            db.execute("INSERT INTO d VALUES (6, 'b')").expect("given up");
            assert_eq!(
                pairs(&db, Family::Text, "SELECT id, code FROM d ORDER BY id"),
                some_pairs(&[(1, "a"), (2, "x"), (3, "y"), (5, "q"), (6, "b")])
            );
        }
    }
}

/// R5.1. An autocommit INSERT of a key a merge is about to land used to get in first and
/// stop the merge half way. It waits now, whichever family, and then it gets its ordinary
/// answer: the merge lands, the key is held, 23505. No key is ever held twice.
#[test]
fn an_autocommit_insert_during_a_merge_waits_and_gets_its_ordinary_answer() {
    for family in FAMILIES {
        for site in [MergeFailSite::Row, MergeFailSite::TableClaimed] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            seed_two_keys_on_a_branch(&db, family);
            let (merged, inserted) = a_writer_waiting_for_a_merge(
                &db,
                "MERGE BRANCH r INTO main",
                site,
                "d",
                || exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s("y")]),
                || {
                    // A reader never waits, and sees none of the table's rows: it
                    // lands whole, or not at all.
                    assert_eq!(
                        pairs(&db, family, "SELECT id, code FROM d WHERE id > 1"),
                        Vec::new(),
                        "[{family:?}] not committed yet"
                    );
                },
            );
            merged.unwrap();
            assert_unique_violation(inserted, family, "a key the merge landed");
            assert_eq!(
                pairs(&db, family, "SELECT id, code FROM d ORDER BY id"),
                some_pairs(&[(1, "a"), (2, "x"), (3, "y")]),
                "[{family:?}]"
            );
            assert_eq!(dense_count(&db), 3, "[{family:?}]");
        }
    }
}

/// An autocommit INSERT between its claim and its row: the key is in the index and
/// nothing has committed it. Validation sees that (55006, nothing changed), and
/// the merge goes through once the claim is gone.
#[test]
fn a_merge_refuses_a_key_an_insert_is_in_the_middle_of_writing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_a_key_on_a_branch(&db, Family::Text);
    let schema = db.storage.catalog().get_table_schema("d").unwrap();
    let phantom = Tuple::new(vec![Value::Int4(99), Value::String("x".to_string())]);
    db.storage
        .art_indexes()
        .reserve_insert_tuple("d", 99, &schema, &phantom)
        .unwrap();
    let message = db
        .execute("MERGE BRANCH r INTO main")
        .expect_err("the key is claimed")
        .to_string();
    assert!(
        message.contains(BRANCH_MERGE_KEY_CLAIMED_MARKER) && message.contains("table d"),
        "{message}"
    );
    assert!(branch_is_active(&db, "r"), "nothing changed");
    assert_eq!(count_keys(&db, "branch:merge:"), 0, "no intent");

    db.storage
        .art_indexes()
        .on_delete_tuple("d", 99, &schema, &phantom)
        .unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM d ORDER BY id", &[])),
        vec![1, 2]
    );
}

/// Rows that swap keys cannot land one at a time; a chain lands in order; a
/// deletion frees its key first. Whatever order the rows arrive in, main's indexes
/// and its dense-int statistics end up as a rebuild would have them.
#[test]
fn a_merge_moves_keys_between_rows_in_any_shape() {
    for family in FAMILIES {
        // A swap.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO d VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        branch(&db, "r");
        ok(&db, family, "UPDATE d SET code = $1 WHERE id = $2", &[s("t"), i(1)]);
        ok(&db, family, "UPDATE d SET code = $1 WHERE id = $2", &[s("a"), i(2)]);
        ok(&db, family, "UPDATE d SET code = $1 WHERE id = $2", &[s("b"), i(1)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            pairs(&db, family, "SELECT id, code FROM d ORDER BY id"),
            some_pairs(&[(1, "b"), (2, "a"), (3, "c")]),
            "[{family:?}] the swap"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM d WHERE code = $1", &[s("a")])),
            vec![2],
            "[{family:?}] the swap, by key"
        );
        assert_eq!(dense_count(&db), 3, "[{family:?}] the swap");
        for held in ["a", "b", "c"] {
            assert_unique_violation(
                exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s(held)]),
                family,
                "the swap",
            );
        }
        db.execute("INSERT INTO d VALUES (9, 't')")
            .expect("the step in between");

        // A chain, both ways round: a key taken from a row that moves on.
        for (first, second) in [(1, 2), (2, 1)] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
                .unwrap();
            let (from_first, from_second) = ("p", "q");
            db.execute(&format!(
                "INSERT INTO d VALUES ({first}, '{from_first}'), ({second}, '{from_second}')"
            ))
            .unwrap();
            branch(&db, "r");
            ok(
                &db,
                family,
                "UPDATE d SET code = $1 WHERE id = $2",
                &[s("z"), i(second)],
            );
            ok(
                &db,
                family,
                "UPDATE d SET code = $1 WHERE id = $2",
                &[s(from_second), i(first)],
            );
            db.execute("USE BRANCH main").unwrap();
            db.execute("MERGE BRANCH r INTO main").unwrap();
            assert_unique_violation(
                exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s("z")]),
                family,
                "the chain's end",
            );
            assert_unique_violation(
                exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s(from_second)]),
                family,
                "the chain's middle",
            );
            db.execute(&format!("INSERT INTO d VALUES (9, '{from_first}')"))
                .expect("the chain's start is free");
            assert_eq!(dense_count(&db), 3, "[{family:?}] the chain");
        }
    }
}

/// B3. A statement counted and past its check when a merge registers completes:
/// it is what the merge is waiting for. `ALTER COLUMN … SET STORAGE` rewrites its
/// rows before it writes the schema, and a refusal of that write left every row
/// holding a reference under a schema that said otherwise. The merge then plans
/// against the new state; here the definition moved, so it is refused.
#[test]
fn set_storage_completes_while_a_merge_waits_for_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, c TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'open'), (2, 'closed')").unwrap();
    branch(&db, "r");
    db.execute("INSERT INTO t VALUES (3, 'on the branch')").unwrap();
    db.execute("USE BRANCH main").unwrap();

    let (statement, merged) = a_merge_registering_during_a_schema_statement(
        &db,
        "ALTER TABLE t ALTER COLUMN c SET STORAGE DICTIONARY",
        "MERGE BRANCH r INTO main",
        "t",
    );
    statement.expect("the statement a merge waits for completes");
    let err = merged.expect_err("the definition moved").to_string();
    assert!(err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER), "{err}");
    assert!(branch_is_active(&db, "r"));
    assert_eq!(count_keys(&db, "branch:merge:"), 0);

    assert_eq!(
        rows(&db, Family::Text, "SELECT c FROM t ORDER BY id", &[])
            .iter()
            .map(|row| text(row.values.first()))
            .collect::<Vec<_>>(),
        vec![Some("open".to_string()), Some("closed".to_string())],
        "the values read back, not raw references"
    );
    db.execute("ALTER TABLE t ADD COLUMN later INT")
        .expect("nothing holds t any more");
}

/// `CREATE INDEX` builds and backfills first and records the index last; a refusal
/// of the record left an index nothing could drop or recreate. A non-unique index
/// does not move the definition, so the merge then goes through, and the row it
/// lands is in the new index.
#[test]
fn create_index_completes_while_a_merge_waits_for_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
    branch(&db, "r");
    db.execute("INSERT INTO t VALUES (3, 7)").unwrap();
    db.execute("USE BRANCH main").unwrap();

    let (statement, merged) = a_merge_registering_during_a_schema_statement(
        &db,
        "CREATE INDEX t_v ON t (v)",
        "MERGE BRANCH r INTO main",
        "t",
    );
    statement.expect("the statement a merge waits for completes");
    merged.expect("then it plans against the new state");

    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM t WHERE v = 7", &[])),
        vec![3],
        "the merged row, through the index"
    );
    db.execute("DROP INDEX t_v").expect("a recorded index");
    db.execute("CREATE INDEX t_v ON t (v)").expect("and its name is free");
}

/// B10. A schema statement on a table the merge does not touch holds nothing up.
#[test]
fn a_merge_does_not_wait_for_a_schema_statement_on_another_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE other (id INT PRIMARY KEY, v INT)").unwrap();
    seed_a_key_on_a_branch(&db, Family::Text);
    let (statement, ()) = during_a_schema_statement(&db, "CREATE INDEX other_v ON other (v)", || {
        db.execute("MERGE BRANCH r INTO main")
            .expect("no overlap, no wait: this thread is what lets the statement go on");
    });
    statement.unwrap();
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM d ORDER BY id", &[])),
        vec![1, 2]
    );
}

/// B9. `DROP TABLE parent` when a pending merge holds a partition child: refused up
/// front. It used to drop the parent and only then be refused at the child.
#[test]
fn a_drop_of_a_partitioned_table_is_refused_whole_when_a_merge_holds_a_child() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")
            .unwrap();
        db.execute("CREATE TABLE child_low PARTITION OF parent FOR VALUES FROM (0) TO (100)")
            .unwrap();
        db.execute("INSERT INTO child_low VALUES (1, 'main')").unwrap();
        branch(&db, "r");
        ok(
            &db,
            family,
            "INSERT INTO child_low VALUES ($1, $2)",
            &[i(2), s("branch")],
        );
        db.execute("USE BRANCH main").unwrap();
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();

        let err = exec(&db, family, "DROP TABLE parent", &[])
            .expect_err("held")
            .to_string();
        assert!(
            err.contains(BRANCH_MERGE_PENDING_MARKER) && err.contains("child_low"),
            "[{family:?}] {err}"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM child_low", &[])),
            vec![1],
            "[{family:?}] the child is there"
        );

        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM child_low ORDER BY id", &[])),
            vec![1, 2],
            "[{family:?}]"
        );
        ok(&db, family, "DROP TABLE parent", &[]);
    }
}

/// The report of an ABORT lists the partition children a merged drop took.
#[test]
fn an_abort_names_the_partition_children_of_a_merged_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE child_low PARTITION OF parent FOR VALUES FROM (0) TO (100)")
        .unwrap();
    branch(&db, "r");
    db.execute("DROP TABLE parent").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_after_steps(0);
    db.execute("MERGE BRANCH r INTO main").unwrap_err();
    let tables: Vec<Option<String>> = rows(&db, Family::Text, "MERGE BRANCH r ABORT", &[])
        .iter()
        .map(|row| text(row.values.first()))
        .collect();
    assert!(tables.contains(&Some("child_low".to_string())), "{tables:?}");
    assert!(tables.contains(&Some("parent".to_string())), "{tables:?}");
}

/// B8. A child forked from the target of an unfinished merge would read through a
/// level part way through being rewritten.
#[test]
fn a_branch_cannot_fork_from_the_target_of_an_unfinished_merge() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    branch(&db, "b");
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    db.execute("USE BRANCH c").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN x INT").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_at(MergeFailSite::Step, 0);
    db.execute("MERGE BRANCH c INTO b").unwrap_err();

    db.execute("USE BRANCH b").unwrap();
    let err = db
        .execute("CREATE BRANCH x AS OF NOW")
        .expect_err("pending")
        .to_string();
    assert!(err.contains(BRANCH_MERGE_PENDING_MARKER), "{err}");
    assert!(db.storage.get_branch("x").is_err(), "no such branch");

    db.execute("USE BRANCH main").unwrap();
    db.execute("MERGE BRANCH c INTO b").unwrap();
    db.execute("USE BRANCH b").unwrap();
    db.execute("CREATE BRANCH x AS OF NOW").expect("once it finished");
}

/// B7. A branch with no fork record was created by an earlier release: nothing says
/// what definition its rows were written for, and rows are copied by position.
/// Fail closed, and say what to do. One that changed no rows has nothing to get
/// wrong.
#[test]
fn a_branch_with_no_fork_record_cannot_merge_rows() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
        db.execute("CREATE BRANCH untouched AS OF NOW").unwrap();
        branch(&db, "r");
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(2), i(2)]);
        db.execute("USE BRANCH main").unwrap();
        for name in ["r", "untouched"] {
            let level = db.storage.get_branch(name).unwrap().branch_id;
            db.storage.delete(&format!("bmeta:{level}:fork").into_bytes()).unwrap();
        }

        let err = db
            .execute("MERGE BRANCH r INTO main")
            .expect_err("fail closed")
            .to_string();
        assert!(
            err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER)
                && err.contains("fork point is not recorded")
                && err.contains("rows of t")
                && err.contains("recreate the branch"),
            "[{family:?}] {err}"
        );
        assert!(branch_is_active(&db, "r"), "[{family:?}]");
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM t", &[])),
            vec![1],
            "[{family:?}]"
        );
        db.execute("MERGE BRANCH untouched INTO main")
            .expect("no rows to place");
    }
}

/// An ancestor that merged into a branch that was since dropped can never reach
/// the target: "merge that branch first" would be impossible advice.
#[test]
fn a_merge_says_so_when_an_ancestors_merge_target_was_dropped() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    branch(&db, "b");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("CREATE BRANCH x AS OF NOW").unwrap();
    db.execute("USE BRANCH x").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.execute("MERGE BRANCH b INTO c").unwrap();
    db.execute("DROP BRANCH c").unwrap();

    let err = db
        .execute("MERGE BRANCH x INTO main")
        .expect_err("out of reach")
        .to_string();
    assert!(
        err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER) && err.contains("has since been dropped"),
        "{err}"
    );
}

/// B4. ABORT after a table landed: no rebuild, because there is nothing to rebuild.
/// Main's PRIMARY KEY, UNIQUE and dense-int statistics describe the rows that are
/// there, and merging the source again is validated from scratch and finishes.
#[test]
fn abort_after_rows_landed_leaves_mains_indexes_matching_its_rows() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE e (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        seed_two_keys_on_a_branch(&db, family);
        db.execute("USE BRANCH r").unwrap();
        ok(&db, family, "INSERT INTO e VALUES ($1, $2)", &[i(5), s("e5")]);
        db.execute("USE BRANCH main").unwrap();
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();

        let named: Vec<Option<String>> = rows(&db, family, "MERGE BRANCH r ABORT", &[])
            .iter()
            .map(|row| text(row.values.first()))
            .collect();
        assert_eq!(
            named,
            vec![Some("d".to_string()), Some("e".to_string())],
            "[{family:?}]"
        );
        assert!(branch_is_active(&db, "r"), "[{family:?}]");
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}]");

        // `d` landed, whole, and stays.
        assert_eq!(dense_count(&db), 3, "[{family:?}]");
        for id in [2, 3] {
            assert_eq!(
                ids(&rows(&db, family, "SELECT id FROM d WHERE id = $1", &[i(id)])),
                vec![i64::from(id)],
                "[{family:?}] in the PRIMARY KEY"
            );
        }
        for code in ["x", "y"] {
            assert_unique_violation(
                exec(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s(code)]),
                family,
                "in the UNIQUE",
            );
        }
        // `e` did not.
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            Vec::<i64>::new(),
            "[{family:?}]"
        );
        ok(&db, family, "INSERT INTO e VALUES ($1, $2)", &[i(6), s("free")]);
        ok(&db, family, "DELETE FROM e WHERE id = $1", &[i(6)]);

        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            vec![5],
            "[{family:?}]"
        );
        assert_eq!(
            pairs(&db, family, "SELECT id, code FROM d ORDER BY id"),
            some_pairs(&[(1, "a"), (2, "x"), (3, "y")]),
            "[{family:?}]"
        );
        assert_eq!(dense_count(&db), 3, "[{family:?}]");
    }
}

/// The site a child process was told to die at, and at which of its ticks.
fn crash_site(name: &str) -> (MergeFailSite, usize) {
    match name {
        "after_intent" => (MergeFailSite::AfterIntent, 0),
        "step" => (MergeFailSite::Step, 0),
        "first_table" => (MergeFailSite::Row, 0),
        "first_table_claimed" => (MergeFailSite::TableClaimed, 0),
        "between_tables" => (MergeFailSite::Row, 1),
        "materialize" => (MergeFailSite::Materialize, 0),
        other => panic!("no such site: {other}"),
    }
}

/// This process dies on the spot, with no destructor and no error path, at that tick.
fn die_at(db: &EmbeddedDatabase, site: &str) {
    let (site, nth) = crash_site(site);
    let ticks = AtomicUsize::new(0);
    db.storage.debug_on_merge_site(
        site,
        Some(Arc::new(move || {
            if ticks.fetch_add(1, Ordering::SeqCst) == nth {
                std::process::abort();
            }
        })),
    );
}

/// `exact` run again as a child process, told where to die. The child must really
/// die there: if it never reached the site it exits 0, and this says so.
fn run_until_the_child_dies(exact: &str, dir: &std::path::Path, site: &str) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(exact)
        .env(CRASH_DIR, dir)
        .env(CRASH_SITE, site)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "{site}: the child never reached it: {status:?}");
}

const CRASH_DIR: &str = "HELIOSDB_B3_MERGE_DIE_DIR";
const CRASH_SITE: &str = "HELIOSDB_B3_MERGE_DIE_SITE";

/// The process dies between the intent and the mark, in a schema step, before a table
/// lands, with its keys claimed and between two tables. A table's commit is one batch,
/// atomic in RocksDB, so these are the points a crash can land on, and what it sees is
/// a table whole or not at all. The next open finishes the merge.
#[test]
fn a_merge_that_dies_at_any_stage_is_finished_at_the_next_open() {
    const THIS: &str = "a_merge_that_dies_at_any_stage_is_finished_at_the_next_open";
    if let (Ok(dir), Ok(site)) = (std::env::var(CRASH_DIR), std::env::var(CRASH_SITE)) {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        seed_merge_of_three_steps(&db, Family::Text);
        die_at(&db, &site);
        let _ = db.execute("MERGE BRANCH r INTO main");
        std::process::exit(0);
    }
    for site in [
        "after_intent",
        "step",
        "first_table",
        "first_table_claimed",
        "between_tables",
    ] {
        let dir = tempfile::tempdir().unwrap();
        run_until_the_child_dies(THIS, dir.path(), site);
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        // The counters were re-seeded from the rows before the finish ran, and on
        // the two earlier sites `a_new` did not exist yet: without the raise, a main
        // INSERT would reuse the row id of the row the finish just landed.
        assert!(
            db.storage.row_counter_high_water("a_new").is_some_and(|mark| mark >= 1),
            "{site}: {:?}",
            db.storage.row_counter_high_water("a_new")
        );
        assert_merge_of_three_steps_finished(&db, Family::Text);
    }
}

/// A merge into a branch that dies between two chunks of an `ADD COLUMN`'s copies:
/// the next open finishes it, and the copies stay copies.
#[test]
fn a_merge_into_a_branch_that_dies_between_chunks_is_finished_at_the_next_open() {
    const THIS: &str = "a_merge_into_a_branch_that_dies_between_chunks_is_finished_at_the_next_open";
    if let (Ok(dir), Ok(site)) = (std::env::var(CRASH_DIR), std::env::var(CRASH_SITE)) {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        seed_put_on_branch(&db, Family::Text);
        db.storage.debug_set_materialize_chunk_rows(1);
        die_at(&db, &site);
        let _ = db.execute("MERGE BRANCH c INTO b");
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    run_until_the_child_dies(THIS, dir.path(), "materialize");
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(count_keys(&db, "branch:merge:"), 0, "finished at the open");
    assert_put_on_branch_finished(&db, Family::Text);
}

/// An open finishes a stopped merge, so there is nothing left for an ABORT to
/// abandon; it says so.
#[test]
fn an_abort_is_refused_when_an_open_finished_the_merge() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        seed_merge_of_three_steps(&db, Family::Text);
        db.storage.debug_fail_merge_after_steps(1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_merge_of_three_steps_finished(&db, Family::Text);
    let err = db.query("MERGE BRANCH r ABORT", &[]).expect_err("finished").to_string();
    assert!(err.contains("no unfinished merge"), "{err}");
}

// ---------------------------------------------------------------------------
// B3 fourth fix pass: a table lands as one ordinary transaction
// ---------------------------------------------------------------------------

/// `d(id PRIMARY KEY, code UNIQUE, v)` on main with (1, 'a', 0), and branch `r` that
/// makes it (1, 'z', 100): the very row a writer on main may want too.
fn seed_a_row_both_write(db: &EmbeddedDatabase, family: Family) {
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE, v INT)")
        .unwrap();
    db.execute("INSERT INTO d VALUES (1, 'a', 0)").unwrap();
    branch(db, "r");
    ok(
        db,
        family,
        "UPDATE d SET code = $1, v = $2 WHERE id = $3",
        &[s("z"), i(100), i(1)],
    );
    db.execute("USE BRANCH main").unwrap();
}

/// Who holds `key`: by a scan of the rows, and through the UNIQUE index. They must
/// always agree, and hold at most one row.
fn holders_of(db: &EmbeddedDatabase, key: &str) -> (Vec<i64>, Vec<i64>) {
    let by_scan = ids(&rows(
        db,
        Family::Text,
        &format!("SELECT id FROM d WHERE code || '' = '{key}' ORDER BY id"),
        &[],
    ));
    let by_key = ids(&rows(
        db,
        Family::Text,
        &format!("SELECT id FROM d WHERE code = '{key}' ORDER BY id"),
        &[],
    ));
    (by_scan, by_key)
}

fn triples(db: &EmbeddedDatabase) -> Vec<(i64, Option<String>, i64)> {
    rows(db, Family::Text, "SELECT id, code, v FROM d ORDER BY id", &[])
        .iter()
        .map(|row| (int(row.values.first()), text(row.values.get(1)), int(row.values.get(2))))
        .collect()
}

/// The largest SCN handed out so far: what an `AS OF SCN` of "now" names.
fn last_scn(db: &EmbeddedDatabase) -> u64 {
    db.storage.snapshot_manager().current_scn().saturating_sub(1)
}

/// Review item 1 (script w3), R5.1: a transaction that writes a row while the merge
/// lands it waits, so its row image comes after the merge's. At READ COMMITTED, the
/// default of every wire client, its statement reads the merged row. It used to read
/// the row first and put the pre-merge image back over the merged one, with the index
/// saying otherwise.
#[test]
fn a_transaction_writing_a_row_a_merge_lands_waits_and_reads_the_merged_row() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_a_row_both_write(&db, family);
        let writer = session(&db, "writer");
        let (merged, wrote) = a_writer_waiting_for_a_merge(
            &db,
            "MERGE BRANCH r INTO main",
            MergeFailSite::TableClaimed,
            "d",
            || {
                db.begin_transaction_for_session(writer)?;
                db.execute_for_session(writer, "UPDATE d SET v = v + 1 WHERE id = 1")
            },
            || {},
        );
        merged.unwrap();
        wrote.unwrap();
        db.commit_transaction_for_session(writer).unwrap();
        assert_eq!(triples(&db), vec![(1, Some("z".to_string()), 101)], "[{family:?}]");
        assert_eq!(holders_of(&db, "z"), (vec![1], vec![1]), "[{family:?}]");
        assert_eq!(holders_of(&db, "a"), (vec![], vec![]), "[{family:?}]");
        assert_unique_violation(
            db.execute("INSERT INTO d VALUES (6, 'z', 0)"),
            Family::Text,
            "the merged key",
        );
        db.execute("INSERT INTO d VALUES (5, 'a', 0)")
            .expect("the key the merge freed");
    }
}

/// The same, at REPEATABLE READ: its snapshot is from before the merge, so the row it
/// read is too, and first-committer-wins refuses its COMMIT (40001). The merged row, its
/// keys and the index stay the merge's, and a later transaction passes.
#[test]
fn a_repeatable_read_commit_over_a_row_a_merge_landed_is_a_serialization_failure() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_a_row_both_write(&db, Family::Text);
    let writer = db.create_session("writer", IsolationLevel::RepeatableRead).unwrap();
    let (merged, wrote) = a_writer_waiting_for_a_merge(
        &db,
        "MERGE BRANCH r INTO main",
        MergeFailSite::TableClaimed,
        "d",
        || {
            db.begin_transaction_for_session(writer)?;
            db.execute_for_session(writer, "UPDATE d SET v = 1 WHERE id = 1")
        },
        || {},
    );
    merged.unwrap();
    wrote.unwrap();
    let err = db
        .commit_transaction_for_session(writer)
        .expect_err("its row image is from before the merge")
        .to_string();
    assert!(err.contains("serialization failure"), "{err}");
    assert_eq!(triples(&db), vec![(1, Some("z".to_string()), 100)]);
    assert_eq!(holders_of(&db, "z"), (vec![1], vec![1]));
    assert_eq!(holders_of(&db, "a"), (vec![], vec![]));

    let later = session(&db, "later");
    db.begin_transaction_for_session(later).unwrap();
    db.execute_for_session(later, "UPDATE d SET v = 2 WHERE id = 1")
        .unwrap();
    db.commit_transaction_for_session(later).unwrap();
    assert_eq!(triples(&db), vec![(1, Some("z".to_string()), 2)]);
}

/// On the wire: the writer's statement does not answer until the merge is done, and a
/// plain READ COMMITTED client never sees an error for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_on_the_wire_waits_for_a_merge_with_no_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_a_row_both_write(&db, Family::Text);
    let s = serve(db).await;
    let (client, task) = connect(s.port).await;

    let (reached, go) = stop_at_site(&s.db, MergeFailSite::TableClaimed, 0);
    let merging = {
        let db = Arc::clone(&s.db);
        std::thread::spawn(move || db.execute("MERGE BRANCH r INTO main"))
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .expect("the merge reaches its commit");
    client.simple_query("BEGIN").await.unwrap();
    let updating = tokio::spawn(async move {
        let answer = client
            .simple_query("UPDATE d SET v = v + 1 WHERE id = 1")
            .await
            .map(|_| ());
        (client, answer)
    });
    let waiting = Arc::clone(&s.db);
    tokio::task::spawn_blocking(move || {
        wait_for("the UPDATE to wait for the table", || {
            waiting.storage.debug_table_lock_waiters("d").1 > 0
        });
    })
    .await
    .unwrap();
    drop(go);
    tokio::task::spawn_blocking(move || merging.join().unwrap())
        .await
        .unwrap()
        .expect("the merge lands");
    s.db.storage.debug_on_merge_site(MergeFailSite::TableClaimed, None);

    let (client, answer) = updating.await.unwrap();
    answer.expect("it waited, then it ran");
    client
        .simple_query("COMMIT")
        .await
        .expect("no stale row image to refuse");
    let row = client
        .query_one("SELECT code, v FROM d WHERE id = 1", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "z");
    assert_eq!(row.get::<_, i32>(1), 101);
    let by_old = client.query("SELECT id FROM d WHERE code = 'a'", &[]).await.unwrap();
    assert!(by_old.is_empty());
    drop(client);
    task.abort();
}

/// Review item 2 (script w10), R5.1. An open transaction deletes a row holding a key the
/// merge wants, which takes the key out of the tree eagerly. The merge waits for it, and
/// gives up with nothing landed; the rollback then finds its key where it left it. The
/// merge used to claim the key in between, and the rollback left two rows holding it.
#[test]
fn a_rollback_around_a_merge_never_duplicates_a_key() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed_two_keys_on_a_branch(&db, family);
        short_lock_timeout(&db);
        let writer = session(&db, "writer");
        db.execute("INSERT INTO d VALUES (8, 'y')").unwrap();
        db.begin_transaction_for_session(writer).unwrap();
        db.execute_for_session(writer, "DELETE FROM d WHERE id = 8").unwrap();
        let err = db.execute("MERGE BRANCH r INTO main").expect_err("an open writer");
        assert_lock_timeout(&err.to_string(), "an open transaction holds it");

        db.rollback_transaction_for_session(writer).unwrap();
        assert_eq!(holders_of(&db, "y"), (vec![8], vec![8]), "[{family:?}]");
        assert_eq!(dense_count(&db), 2, "[{family:?}] nothing landed");

        // Committed now, so it is what validation refuses, and when it is gone the
        // merge finishes.
        let err = db.execute("MERGE BRANCH r INTO main").expect_err("a duplicate");
        assert!(is_unique_violation(&err), "[{family:?}] {err}");
        ok(&db, family, "DELETE FROM d WHERE id = $1", &[i(8)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(holders_of(&db, "y"), (vec![3], vec![3]), "[{family:?}]");
        assert_eq!(holders_of(&db, "x"), (vec![2], vec![2]), "[{family:?}]");
        assert_eq!(dense_count(&db), 3, "[{family:?}]");
    }
}

/// Review item 3 (script w9). Rows swapping keys land as one: at no moment, before
/// their keys are claimed or between the claim and the commit, is a key free for an
/// INSERT to take. It used to release them all first, an INSERT got in, and the
/// put-back failed: two rows with one key. R5.1: the INSERT waits for the merge, and
/// after that the key has its new owner.
#[test]
fn a_key_being_swapped_always_has_an_owner() {
    for site in [MergeFailSite::Row, MergeFailSite::TableClaimed] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        short_lock_timeout(&db);
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO d VALUES (1, 'a'), (2, 'b')").unwrap();
        branch(&db, "r");
        db.execute("UPDATE d SET code = 't' WHERE id = 1").unwrap();
        db.execute("UPDATE d SET code = 'a' WHERE id = 2").unwrap();
        db.execute("UPDATE d SET code = 'b' WHERE id = 1").unwrap();
        db.execute("USE BRANCH main").unwrap();

        let (merged, refusals) = during_a_merge(&db, "MERGE BRANCH r INTO main", site, 0, || {
            (
                db.execute("INSERT INTO d VALUES (9, 'a')"),
                db.execute("INSERT INTO d VALUES (9, 'b')"),
            )
        });
        assert_lock_timeout(
            &refusals.0.unwrap_err().to_string(),
            "MERGE BRANCH r INTO main holds it",
        );
        assert_lock_timeout(
            &refusals.1.unwrap_err().to_string(),
            "MERGE BRANCH r INTO main holds it",
        );
        merged.expect("nothing stood in its way");
        assert_unique_violation(db.execute("INSERT INTO d VALUES (9, 'a')"), Family::Text, "a");
        assert_unique_violation(db.execute("INSERT INTO d VALUES (9, 'b')"), Family::Text, "b");
        assert_eq!(
            pairs(&db, Family::Text, "SELECT id, code FROM d ORDER BY id"),
            some_pairs(&[(1, "b"), (2, "a")]),
            "{site:?}"
        );
        assert_eq!(holders_of(&db, "a"), (vec![2], vec![2]), "{site:?}");
        assert_eq!(holders_of(&db, "b"), (vec![1], vec![1]), "{site:?}");
    }
}

/// A table whose commit does not happen: every key the claim moved or entered goes back
/// to the row that holds it, a swap included, and the rerun lands it.
#[test]
fn a_commit_that_does_not_happen_puts_every_key_back() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO d VALUES (1, 'a'), (2, 'b')").unwrap();
    branch(&db, "r");
    db.execute("UPDATE d SET code = 't' WHERE id = 1").unwrap();
    db.execute("UPDATE d SET code = 'a' WHERE id = 2").unwrap();
    db.execute("UPDATE d SET code = 'b' WHERE id = 1").unwrap();
    db.execute("INSERT INTO d VALUES (3, 'c')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.storage.debug_fail_merge_at(MergeFailSite::TableClaimed, 0);
    db.execute("MERGE BRANCH r INTO main").unwrap_err();

    assert_eq!(holders_of(&db, "a"), (vec![1], vec![1]));
    assert_eq!(holders_of(&db, "b"), (vec![2], vec![2]));
    assert_eq!(holders_of(&db, "c"), (vec![], vec![]));
    assert_eq!(dense_count(&db), 2);
    assert_unique_violation(db.execute("INSERT INTO d VALUES (9, 'a')"), Family::Text, "row 1's key");
    assert_unique_violation(db.execute("INSERT INTO d VALUES (9, 'b')"), Family::Text, "row 2's key");

    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(holders_of(&db, "a"), (vec![2], vec![2]));
    assert_eq!(holders_of(&db, "b"), (vec![1], vec![1]));
    assert_eq!(holders_of(&db, "c"), (vec![3], vec![3]));
    assert_eq!(dense_count(&db), 3);
}

/// R5.1: nothing SQL does can reach a table's keys while its merge holds it, so the
/// claim's refusal is a fail-safe, and this test takes the key in the tree itself. A
/// foreign holder stops the table with nothing of it landed: the keys the claim had
/// already moved or entered go back, in every tree, and a reader never sees a row of
/// it. Whole or not at all.
#[test]
fn a_refused_table_lands_nothing_and_its_keys_go_back() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE, other TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO d VALUES (1, 'a', 'p'), (2, 'b', 'q')").unwrap();
    branch(&db, "r");
    db.execute("UPDATE d SET code = 'c', other = 'r' WHERE id = 1").unwrap();
    db.execute("INSERT INTO d VALUES (3, 'd', 's')").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // Taken after validation, so only the claim can see it; the tree of `other` is
    // the one that refuses, whichever order the trees go in.
    let schema = db.storage.catalog().get_table_schema("d").unwrap();
    let holder = Tuple::new(vec![
        Value::Int4(8),
        Value::String("zz".to_string()),
        Value::String("s".to_string()),
    ]);
    let (merged, ()) = during_a_merge(&db, "MERGE BRANCH r INTO main", MergeFailSite::Row, 0, || {
        db.storage
            .art_indexes()
            .reserve_insert_tuple("d", 8, &schema, &holder)
            .unwrap();
    });
    let err = merged.expect_err("held").to_string();
    assert!(err.contains(BRANCH_MERGE_KEY_CLAIMED_MARKER), "{err}");

    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM d ORDER BY id", &[])),
        vec![1, 2]
    );
    db.storage
        .art_indexes()
        .on_delete_tuple("d", 8, &schema, &holder)
        .unwrap();
    // Row 1 still holds what it holds, and the keys it wanted are free again.
    assert_eq!(holders_of(&db, "a"), (vec![1], vec![1]));
    assert_eq!(holders_of(&db, "c"), (vec![], vec![]));
    assert_unique_violation(
        db.execute("INSERT INTO d VALUES (20, 'a', 'x1')"),
        Family::Text,
        "row 1's code",
    );
    assert_unique_violation(
        db.execute("INSERT INTO d VALUES (20, 'x2', 'p')"),
        Family::Text,
        "row 1's other",
    );
    db.execute("INSERT INTO d VALUES (20, 'c', 'r')")
        .expect("what the merge wanted is free");
    assert_eq!(dense_count(&db), 3);
}

/// Review item 4 (scripts w4a, w4b). A merge creates `t`, and stops in `a` before any
/// row of `t` landed; the process dies; the open cannot finish it. Main serves `t`
/// meanwhile, and a row it commits must not take a row id the merge will land: it used
/// to get id 1, and the merged row 1 silently replaced it. The counter passes the
/// source's rows in the create step, durably, on a rerun too.
#[test]
fn a_row_main_commits_to_a_created_table_survives_the_pending_merge() {
    const THIS: &str = "a_row_main_commits_to_a_created_table_survives_the_pending_merge";
    if let Ok(dir) = std::env::var(CRASH_DIR) {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        db.execute("CREATE TABLE a (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO a VALUES (1, 'a1'), (2, 'a2')").unwrap();
        branch(&db, "r");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, w TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (100, 'b1'), (200, 'b2')").unwrap();
        db.execute("UPDATE a SET code = 'z2' WHERE id = 2").unwrap();
        db.execute("USE BRANCH main").unwrap();
        // The merge stops before `a` lands, with `t` created. A pending merge leaves
        // main's DML open, so a row takes the key it wants: it cannot finish at the
        // next open.
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
        if db.execute("INSERT INTO a VALUES (900, 'z2')").is_ok() {
            std::process::abort();
        }
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    run_until_the_child_dies(THIS, dir.path(), "w4");

    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(count_keys(&db, "branch:merge:"), 1, "the open could not finish it");
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM t", &[])),
        Vec::<i64>::new(),
        "created, and none of its rows landed"
    );
    db.execute("INSERT INTO t VALUES (500, 'a row main committed')")
        .unwrap();

    db.execute("DELETE FROM a WHERE id = 900").unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM t ORDER BY id", &[])),
        vec![100, 200, 500]
    );
    assert_eq!(
        ids(&rows(&db, Family::Text, "SELECT id FROM t WHERE id = 500", &[])),
        vec![500]
    );
    assert_eq!(
        int(rows(&db, Family::Text, "SELECT COUNT(*) FROM t", &[])[0].values.first()),
        3
    );
}

/// A table that landed is done with. A rerun leaves it alone, so what main changed in
/// it since stays changed, and validation does not hold main's later rows against it.
#[test]
fn a_rerun_leaves_a_table_that_landed_alone() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE e (id INT PRIMARY KEY)").unwrap();
        seed_two_keys_on_a_branch(&db, family);
        db.execute("USE BRANCH r").unwrap();
        ok(&db, family, "INSERT INTO e VALUES ($1)", &[i(1)]);
        db.execute("USE BRANCH main").unwrap();
        // `d`, then `e`: stopped between them.
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 1);
        db.execute("MERGE BRANCH r INTO main").unwrap_err();
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            Vec::<i64>::new(),
            "[{family:?}]"
        );

        ok(&db, family, "UPDATE d SET code = $1 WHERE id = $2", &[s("mine"), i(2)]);
        ok(&db, family, "INSERT INTO d VALUES ($1, $2)", &[i(9), s("x")]);

        db.execute("MERGE BRANCH r INTO main")
            .expect("`d` is not looked at again");
        assert_eq!(
            pairs(&db, family, "SELECT id, code FROM d ORDER BY id"),
            some_pairs(&[(1, "a"), (2, "mine"), (3, "y"), (9, "x")]),
            "[{family:?}]"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM e", &[])),
            vec![1],
            "[{family:?}]"
        );
        assert_eq!(count_keys(&db, "branch:merge:"), 0, "[{family:?}]");
    }
}

/// Pre-existing, closed by the commit (script w7a, w7b): a merge wrote no logical WAL,
/// so after an earlier clean close left a checkpoint, a crash replayed older entries
/// over its rows: the merge was reverted, with the source Merged and a UNIQUE key on
/// two rows. Its rows are in the WAL now, after them.
#[test]
fn a_finished_merge_survives_the_replay_of_older_wal_entries() {
    const THIS: &str = "a_finished_merge_survives_the_replay_of_older_wal_entries";
    if let Ok(dir) = std::env::var(CRASH_DIR) {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        let writer = session(&db, "writer");
        db.begin_transaction_for_session(writer).unwrap();
        db.execute_for_session(writer, "UPDATE d SET v = 5 WHERE id = 1")
            .unwrap();
        db.commit_transaction_for_session(writer).unwrap();
        branch(&db, "r");
        db.execute("UPDATE d SET code = 'z', v = 100 WHERE id = 1").unwrap();
        db.execute("INSERT INTO d VALUES (2, 'a', 2)").unwrap();
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        std::process::abort();
    }
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE, v INT)")
            .unwrap();
        db.execute("INSERT INTO d VALUES (1, 'a', 0)").unwrap();
    }
    run_until_the_child_dies(THIS, dir.path(), "w7");

    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(
        triples(&db),
        vec![(1, Some("z".to_string()), 100), (2, Some("a".to_string()), 2)]
    );
    assert_eq!(holders_of(&db, "a"), (vec![2], vec![2]));
    assert_eq!(holders_of(&db, "z"), (vec![1], vec![1]));
    assert!(matches!(
        db.storage.get_branch("r").unwrap().state,
        BranchState::Merged { .. }
    ));
    assert_eq!(count_keys(&db, "branch:merge:"), 0);
}

/// Pre-existing, closed by the commit: a merge wrote no versions. `AS OF` before it
/// now shows the replaced row's old value and not the row it inserted, and `AS OF`
/// after it shows the new value. A transaction begun after it reads the merged rows
/// (a scan used to answer with the pre-merge row); one begun before it does not.
///
/// A row the merge deleted is absent from `AS OF` at any point, exactly as a row main
/// itself deleted is: `scan_table_at_snapshot` starts from the current rows, and that
/// is B4's to change (`B4_branch_fork_point.md`).
#[test]
fn time_travel_and_snapshots_see_a_merge_like_any_commit() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    let writer = session(&db, "writer");
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "INSERT INTO d VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    db.commit_transaction_for_session(writer).unwrap();
    branch(&db, "r");
    db.execute("UPDATE d SET code = 'z' WHERE id = 1").unwrap();
    db.execute("DELETE FROM d WHERE id = 2").unwrap();
    db.execute("INSERT INTO d VALUES (4, 'd')").unwrap();
    db.execute("USE BRANCH main").unwrap();

    let before_the_merge = db.create_session("old", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(before_the_merge).unwrap();
    let read_in = |sid: heliosdb_nano::session::SessionId| -> Vec<(i64, Option<String>)> {
        let mut found: Vec<(i64, Option<String>)> = db
            .query_in_session(sid, "SELECT id, code FROM d", &[])
            .unwrap()
            .iter()
            .map(|row| (int(row.values.first()), text(row.values.get(1))))
            .collect();
        found.sort();
        found
    };
    let at = |scn: u64| -> Vec<(i64, Option<String>)> {
        let mut found = pairs(&db, Family::Text, &format!("SELECT id, code FROM d AS OF SCN {scn}"));
        found.sort();
        found
    };
    assert_eq!(read_in(before_the_merge).len(), 3, "it has read its snapshot");

    let scn_before = last_scn(&db);
    db.execute("MERGE BRANCH r INTO main").unwrap();
    let scn_after = last_scn(&db);
    assert!(scn_after > scn_before, "the merge's commit is a snapshot");

    assert_eq!(at(scn_before), some_pairs(&[(1, "a"), (3, "c")]));
    assert_eq!(at(scn_after), some_pairs(&[(1, "z"), (3, "c"), (4, "d")]));

    assert_eq!(
        read_in(before_the_merge),
        some_pairs(&[(1, "a"), (3, "c")]),
        "a snapshot from before the merge"
    );
    let after_the_merge = db.create_session("new", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(after_the_merge).unwrap();
    assert_eq!(
        read_in(after_the_merge),
        some_pairs(&[(1, "z"), (3, "c"), (4, "d")]),
        "a snapshot from after it"
    );
    db.rollback_transaction_for_session(before_the_merge).unwrap();
    db.rollback_transaction_for_session(after_the_merge).unwrap();
}

/// Review item 5 (script s3): a table created on a branch with `STORAGE COLUMNAR`. Its
/// rows reach the column store: every shape of read agrees. `SELECT v` used to answer
/// NULL for every row.
#[test]
fn a_columnar_table_a_merge_creates_reads_right() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        branch(&db, "r");
        ok(
            &db,
            family,
            "CREATE TABLE c (id INT PRIMARY KEY, v INT STORAGE COLUMNAR)",
            &[],
        );
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(1), i(10)]);
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(2), i(20)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_columnar_reads(&db, family, &[(1, 10), (2, 20)]);
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(3), i(30)]);
        assert_columnar_reads(&db, family, &[(1, 10), (2, 20), (3, 30)]);
    }
}

/// Review item 5 (script s4): an existing columnar table, through an update, a delete
/// and an insert in one merge. `SELECT v` and `SUM(v)` used to answer from the column
/// store as if nothing had been merged.
#[test]
fn a_columnar_table_a_merge_changes_reads_right() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, v INT STORAGE COLUMNAR)")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        assert_columnar_reads(&db, family, &[(1, 10), (2, 20), (3, 30)]);
        branch(&db, "r");
        ok(&db, family, "UPDATE c SET v = $1 WHERE id = $2", &[i(99), i(1)]);
        ok(&db, family, "DELETE FROM c WHERE id = $1", &[i(2)]);
        ok(&db, family, "INSERT INTO c VALUES ($1, $2)", &[i(4), i(40)]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_columnar_reads(&db, family, &[(1, 99), (3, 30), (4, 40)]);
    }
}

fn assert_columnar_reads(db: &EmbeddedDatabase, family: Family, expected: &[(i64, i64)]) {
    let wide: Vec<(i64, i64)> = rows(db, family, "SELECT id, v FROM c ORDER BY id", &[])
        .iter()
        .map(|row| (int(row.values.first()), int(row.values.get(1))))
        .collect();
    assert_eq!(wide, expected, "[{family:?}] through the rows");
    let mut narrow = ids(&rows(db, family, "SELECT v FROM c", &[]));
    narrow.sort_unstable();
    let mut values: Vec<i64> = expected.iter().map(|(_, v)| *v).collect();
    values.sort_unstable();
    assert_eq!(narrow, values, "[{family:?}] through the column store");
    let sum: i64 = values.iter().sum();
    let summed = rows(db, family, "SELECT SUM(v) FROM c", &[]);
    assert_eq!(
        summed[0]
            .values
            .first()
            .map(|value| value.to_string().parse::<f64>().unwrap()),
        Some(sum as f64),
        "[{family:?}] SUM"
    );
    assert_eq!(
        int(rows(db, family, "SELECT COUNT(v) FROM c", &[])[0].values.first()),
        i64::try_from(expected.len()).unwrap(),
        "[{family:?}] COUNT(v)"
    );
    for (id, v) in expected {
        let id_at_v = rows(db, Family::Text, &format!("SELECT id FROM c WHERE v = {v}"), &[]);
        assert_eq!(ids(&id_at_v), vec![*id], "[{family:?}] filtered on the column");
    }
}

/// Script s4's other half: dictionary and content-addressed columns, a composite
/// UNIQUE and a secondary index, through an update, a delete and an insert.
#[test]
fn side_storage_and_every_kind_of_index_survive_a_merge() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE s (id INT PRIMARY KEY, a TEXT STORAGE DICTIONARY, b TEXT STORAGE CONTENT_ADDRESSED, \
             k INT, UNIQUE (a, k))",
        )
        .unwrap();
        db.execute("CREATE INDEX s_k ON s (k)").unwrap();
        db.execute("INSERT INTO s VALUES (1, 'x', 'bbb', 1), (2, 'y', 'ccc', 2), (3, 'x', 'ddd', 3)")
            .unwrap();
        branch(&db, "r");
        ok(
            &db,
            family,
            "UPDATE s SET a = $1, k = $2 WHERE id = $3",
            &[s("w"), i(20), i(2)],
        );
        ok(&db, family, "DELETE FROM s WHERE id = $1", &[i(3)]);
        ok(
            &db,
            family,
            "INSERT INTO s VALUES ($1, $2, $3, $4)",
            &[i(4), s("x"), s("eee"), i(3)],
        );
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();

        let merged: Vec<(i64, Option<String>, Option<String>, i64)> =
            rows(&db, family, "SELECT id, a, b, k FROM s ORDER BY id", &[])
                .iter()
                .map(|row| {
                    (
                        int(row.values.first()),
                        text(row.values.get(1)),
                        text(row.values.get(2)),
                        int(row.values.get(3)),
                    )
                })
                .collect();
        assert_eq!(
            merged,
            vec![
                (1, Some("x".to_string()), Some("bbb".to_string()), 1),
                (2, Some("w".to_string()), Some("ccc".to_string()), 20),
                (4, Some("x".to_string()), Some("eee".to_string()), 3),
            ],
            "[{family:?}]"
        );
        for (k, expected) in [(20, vec![2_i64]), (2, vec![]), (3, vec![4])] {
            assert_eq!(
                ids(&rows(&db, family, "SELECT id FROM s WHERE k = $1", &[i(k)])),
                expected,
                "[{family:?}] the secondary index, k = {k}"
            );
        }
        assert_eq!(
            ids(&rows(
                &db,
                family,
                "SELECT id FROM s WHERE a = $1 AND k = $2",
                &[s("w"), i(20)]
            )),
            vec![2],
            "[{family:?}]"
        );
        assert_unique_violation(
            exec(
                &db,
                family,
                "INSERT INTO s VALUES ($1, $2, $3, $4)",
                &[i(9), s("w"), s("q"), i(20)],
            ),
            family,
            "the composite UNIQUE's new key",
        );
        exec(
            &db,
            family,
            "INSERT INTO s VALUES ($1, $2, $3, $4)",
            &[i(9), s("y"), s("q"), i(2)],
        )
        .expect("the composite UNIQUE's old key is free");
        assert_eq!(
            int(rows(&db, family, "SELECT COUNT(*) FROM s", &[])[0].values.first()),
            4,
            "[{family:?}]"
        );
    }
}

/// `SELECT … LIMIT n` on a branch answers with the branch's rows, in both executor
/// families. A pushdown walked main's keys: an overlay's rows were not there at all,
/// and what the branch deleted was.
#[test]
fn limit_on_a_branch_reads_the_branchs_rows() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        branch(&db, "r");
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(1)]);
        ok(&db, family, "INSERT INTO t VALUES ($1)", &[i(4)]);
        ok(&db, family, "CREATE TABLE fresh (id INT PRIMARY KEY)", &[]);
        ok(&db, family, "INSERT INTO fresh VALUES ($1)", &[i(7)]);

        let mut limited = ids(&rows(&db, family, "SELECT * FROM t LIMIT 10", &[]));
        limited.sort_unstable();
        assert_eq!(limited, vec![2, 3, 4], "[{family:?}]");
        assert_eq!(
            ids(&rows(&db, family, "SELECT id FROM t LIMIT 2", &[])).len(),
            2,
            "[{family:?}]"
        );
        assert_eq!(
            ids(&rows(&db, family, "SELECT * FROM fresh LIMIT 11 OFFSET 0", &[])),
            vec![7],
            "[{family:?}]"
        );
        assert!(
            rows(&db, family, "SELECT * FROM fresh LIMIT 11 OFFSET 1", &[]).is_empty(),
            "[{family:?}]"
        );

        db.execute("USE BRANCH main").unwrap();
        let mut on_main = ids(&rows(&db, family, "SELECT * FROM t LIMIT 10", &[]));
        on_main.sort_unstable();
        assert_eq!(on_main, vec![1, 2, 3], "[{family:?}] main is untouched");
    }
}

/// `PARENT` is what `AGENTS.md` documents, and the same clause as `FROM`, in both
/// spellings of the statement. It used to be accepted and ignored: the branch forked
/// from the session's branch.
#[test]
fn create_branch_honours_parent_like_from() {
    for spelling in ["CREATE DATABASE BRANCH", "CREATE BRANCH"] {
        for clause in ["PARENT", "FROM", "parent"] {
            for as_of in [" AS OF NOW", ""] {
                let db = EmbeddedDatabase::new_in_memory().unwrap();
                db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
                branch(&db, "r1");
                db.execute("INSERT INTO t VALUES (2)").unwrap();
                db.execute("USE BRANCH main").unwrap();

                db.execute(&format!("{spelling} r2 {clause} r1{as_of}")).unwrap();
                let (parent, child) = (
                    db.storage.get_branch("r1").unwrap(),
                    db.storage.get_branch("r2").unwrap(),
                );
                assert_eq!(child.parent_id, Some(parent.branch_id), "{spelling} {clause}{as_of}");
                db.execute("USE BRANCH r2").unwrap();
                assert_eq!(
                    ids(&rows(&db, Family::Text, "SELECT id FROM t", &[])),
                    vec![2],
                    "{spelling} {clause}{as_of}: it reads through r1"
                );
            }
        }
    }
    // A parent that does not exist is an error, not main.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE DATABASE BRANCH r2 PARENT nobody AS OF NOW")
        .expect_err("no such parent");
}

/// The clauses are words, with any whitespace between them: `FROM<newline>p` used to
/// fork from main, silently. And what is not a clause is refused, not ignored.
#[test]
fn create_branch_reads_its_clauses_by_word_and_refuses_the_rest() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    branch(&db, "r1");
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let parent = db.storage.get_branch("r1").unwrap().branch_id;

    for (n, clauses) in [
        "PARENT\nr1",
        "PARENT\tr1",
        "FROM\nr1",
        "FROM\tr1 ",
        "  FROM  r1  AS\nOF\n\tNOW",
        "AS OF NOW\nFROM r1",
        "PARENT r1 WITH (region = 'eu');",
        "FROM r1 -- the parent\n AS OF NOW",
    ]
    .iter()
    .enumerate()
    {
        for spelling in ["CREATE DATABASE BRANCH", "CREATE BRANCH"] {
            let name = format!("c{n}_{}", spelling.len());
            db.execute(&format!("{spelling} {name} {clauses}")).unwrap();
            assert_eq!(
                db.storage.get_branch(&name).unwrap().parent_id,
                Some(parent),
                "{spelling} … {clauses:?}"
            );
        }
    }

    for clauses in [
        "PARNT r1",
        "r1",
        "PARENT",
        "FROM",
        "FROM r1 junk",
        "FROM r1 PARENT r1",
        "AS OF NOW junk",
        "AS OF NOW AS OF NOW",
        "AS r1",
        "AS OF",
        "WITH",
        "WITH (region = 'eu') junk",
        "FROM r1; DROP BRANCH r1",
    ] {
        for spelling in ["CREATE DATABASE BRANCH", "CREATE BRANCH"] {
            let err = db.execute(&format!("{spelling} refused {clauses}"));
            assert!(err.is_err(), "{spelling} … {clauses:?}");
            assert!(db.storage.get_branch("refused").is_err(), "{spelling} … {clauses:?}");
        }
    }
}

/// A result a reader caches while a merge runs must not outlive a table landing: the
/// statement's own invalidation runs only if it succeeds, and here a later table stops
/// it. `SELECT SUM(v)` went on answering 0 until some other DML cleared the cache.
#[test]
fn a_result_cached_during_a_merge_does_not_outlive_the_table_landing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE aa (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO aa VALUES (1, 0), (2, 0)").unwrap();
    db.execute("CREATE TABLE zz (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO zz VALUES (1, 'a')").unwrap();
    branch(&db, "r");
    db.execute("UPDATE aa SET v = 100").unwrap();
    db.execute("UPDATE zz SET code = 'want' WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
    let sum = || {
        rows(&db, Family::Text, "SELECT SUM(v) FROM aa", &[])[0]
            .values
            .first()
            .map(|value| value.to_string().parse::<f64>().unwrap())
    };
    let hundreds = || {
        int(rows(&db, Family::Text, "SELECT COUNT(*) FROM aa WHERE v = 100", &[])[0]
            .values
            .first())
    };

    let zz_schema = db.storage.catalog().get_table_schema("zz").unwrap();
    let holder = Tuple::new(vec![Value::Int4(1000), Value::String("want".to_string())]);
    let (merged, ()) = during_a_merge(&db, "MERGE BRANCH r INTO main", MergeFailSite::Row, 0, || {
        // `aa` goes first and lands; this stops the merge at `zz`, as nothing SQL does
        // can while the merge holds it (R5.1).
        db.storage
            .art_indexes()
            .reserve_insert_tuple("zz", 1000, &zz_schema, &holder)
            .unwrap();
        // The second sighting admits a result to the cache, the third reads it.
        for _ in 0..3 {
            assert_eq!(sum(), Some(0.0));
            assert_eq!(hundreds(), 0);
        }
    });
    let err = merged.expect_err("`zz` is stopped").to_string();
    assert!(err.contains(BRANCH_MERGE_KEY_CLAIMED_MARKER), "{err}");

    assert_eq!(sum(), Some(200.0), "`aa` landed");
    assert_eq!(hundreds(), 2, "`aa` landed");
}

/// Scripts w16, w17: an autocommit UPDATE racing the rewrite of an `ADD COLUMN`, in a
/// merge and in main's own statement. The UPDATE's read-modify-write could put a narrow
/// row back after the backfill passed it, so it read `x IS NULL` instead of its
/// default, and a primary-key read could fail. R5.1: the UPDATEs wait for the rewrite,
/// which waits for them: every row keeps both.
#[test]
fn an_add_column_backfill_survives_autocommit_updates() {
    /// Stops the writers on every way out of the scope joining them, an unwinding
    /// statement included: nothing else would, and the scope would wait for ever.
    struct StopWriters<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for StopWriters<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    const ROWS: i32 = 3000;
    const WRITERS_RUN_AT_MOST: Duration = Duration::from_secs(300);
    // A statement that never returns: no flag stops that. Past this the process ends,
    // with a message libtest cannot capture, rather than hold the build lock.
    let (_disarm, disarmed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(Duration::from_secs(900)) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "\nwatchdog: an_add_column_backfill_survives_autocommit_updates still running after 900 s"
            );
            std::process::abort();
        }
    });
    for through_a_merge in [true, false] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        for start in (1..=ROWS).step_by(500) {
            let values: Vec<String> = (start..start + 500).map(|id| format!("({id}, 0)")).collect();
            db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
                .unwrap();
        }
        let statement = if through_a_merge {
            branch(&db, "r");
            db.execute("ALTER TABLE t ADD COLUMN x INT DEFAULT 5").unwrap();
            db.execute("USE BRANCH main").unwrap();
            "MERGE BRANCH r INTO main"
        } else {
            "ALTER TABLE t ADD COLUMN x INT DEFAULT 5"
        };
        let done = std::sync::atomic::AtomicBool::new(false);
        let out_of_time = std::sync::atomic::AtomicBool::new(false);
        let deadline = std::time::Instant::now() + WRITERS_RUN_AT_MOST;
        std::thread::scope(|scope| {
            let _stop = StopWriters(&done);
            for row in [1, 500, 1000, 1500, 2000, 2500, 2999] {
                let (db, done, out_of_time) = (&db, &done, &out_of_time);
                scope.spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        if std::time::Instant::now() >= deadline {
                            out_of_time.store(true, Ordering::Release);
                            return;
                        }
                        let _ = db.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {row}"));
                    }
                });
            }
            db.execute(statement).unwrap();
        });
        assert!(
            !out_of_time.load(Ordering::Acquire),
            "`{statement}` had not finished after {} s beside the writers",
            WRITERS_RUN_AT_MOST.as_secs()
        );
        assert_eq!(
            int(rows(&db, Family::Text, "SELECT COUNT(*) FROM t WHERE x = 5", &[])[0]
                .values
                .first()),
            i64::from(ROWS),
            "through a merge: {through_a_merge}"
        );
        for row in [1, 500, 1000, 1500, 2000, 2500, 2999] {
            assert_eq!(
                ids(&rows(
                    &db,
                    Family::Text,
                    &format!("SELECT x FROM t WHERE id = {row}"),
                    &[]
                )),
                vec![5],
                "through a merge: {through_a_merge}"
            );
        }
    }
}
