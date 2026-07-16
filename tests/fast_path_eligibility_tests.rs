//! R1.2 — fast-path eligibility hardening (word-boundary, literal-aware
//! bail-word checks).
//!
//! Before R1.2 the fast-path eligibility checks used raw byte-substring
//! matching, so common identifiers silently knocked statements off the
//! parse-skipping fast paths (`points` contains "IN", `order_id` contains
//! "OR"/"ORDER", table `default_settings` contains "DEFAULT", a TEXT literal
//! containing "select" killed INSERT, …).
//!
//! These tests assert *correct execution end-state* for both sides of the
//! matrix:
//! - statements that should now be fast-path eligible must produce exactly
//!   the same results as the planner would (we assert literal expected
//!   values), and
//! - statements that MUST still bail (real keywords) must keep executing
//!   correctly through the planner — if a fast path wrongly claimed them,
//!   the assertions on row contents would fail.

mod test_helpers;

use heliosdb_nano::{Config, EmbeddedDatabase, Result, Value};
use test_helpers::create_test_db;

// ============================================================================
// Positive matrix: shapes that should no longer bail off the fast paths
// ============================================================================

#[test]
fn insert_into_table_named_default_settings() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE default_settings (id INT PRIMARY KEY, name TEXT)")?;

    // "default_settings" contains the substring DEFAULT — pre-R1.2 this fell
    // off the fast insert path. Either way the row must land correctly.
    let n = db.execute("INSERT INTO default_settings (id, name) VALUES (1, 'theme')")?;
    assert_eq!(n, 1);
    db.execute("INSERT INTO default_settings (id, name) VALUES (2, 'lang')")?;

    let rows = db.query("SELECT * FROM default_settings WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(1));
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("theme".to_string()));

    let all = db.query("SELECT * FROM default_settings", &[])?;
    assert_eq!(all.len(), 2);
    Ok(())
}

#[test]
fn insert_literals_containing_keywords_round_trip() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE msgs (id INT PRIMARY KEY, body TEXT)")?;

    let cases: &[(i32, &str, &str)] = &[
        // (id, SQL literal spelling, expected stored string)
        (1, "'please select me'", "please select me"),
        (2, "'the default value'", "the default value"),
        (3, "'it''s an order'", "it's an order"), // '' escape + ORDER/OR
        (4, "'to be or not, and in between'", "to be or not, and in between"),
        (5, "'on conflict do nothing'", "on conflict do nothing"),
        (6, "'returning soon'", "returning soon"),
        (7, "'tell me where'", "tell me where"),
    ];

    for (id, lit, _) in cases {
        let n = db.execute(&format!("INSERT INTO msgs (id, body) VALUES ({id}, {lit})"))?;
        assert_eq!(n, 1, "INSERT with literal {lit} must insert exactly one row");
    }

    for (id, lit, expected) in cases {
        let rows = db.query(&format!("SELECT * FROM msgs WHERE id = {id}"), &[])?;
        assert_eq!(rows.len(), 1, "row for literal {lit} must exist");
        assert_eq!(
            rows[0].get(1).unwrap(),
            &Value::String((*expected).to_string()),
            "literal {lit} must round-trip exactly"
        );
    }
    Ok(())
}

#[test]
fn update_where_points_eq_literal() -> Result<()> {
    // "points" contains the substring IN — pre-R1.2 this bailed try_fast_update.
    let db = create_test_db()?;
    db.execute("CREATE TABLE scores (points INT PRIMARY KEY, val TEXT)")?;
    db.execute("INSERT INTO scores (points, val) VALUES (5, 'a')")?;
    db.execute("INSERT INTO scores (points, val) VALUES (6, 'b')")?;

    let n = db.execute("UPDATE scores SET val = 'z' WHERE points = 5")?;
    assert_eq!(n, 1);

    let hit = db.query("SELECT * FROM scores WHERE points = 5", &[])?;
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].get(1).unwrap(), &Value::String("z".to_string()));

    // The other row must be untouched.
    let other = db.query("SELECT * FROM scores WHERE points = 6", &[])?;
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].get(1).unwrap(), &Value::String("b".to_string()));
    Ok(())
}

#[test]
fn update_where_order_id_eq_literal() -> Result<()> {
    // "order_id" contains the substring OR — pre-R1.2 this bailed.
    let db = create_test_db()?;
    db.execute("CREATE TABLE orders (order_id INT PRIMARY KEY, status TEXT)")?;
    db.execute("INSERT INTO orders (order_id, status) VALUES (3, 'open')")?;
    db.execute("INSERT INTO orders (order_id, status) VALUES (4, 'open')")?;

    let n = db.execute("UPDATE orders SET status = 'shipped' WHERE order_id = 3")?;
    assert_eq!(n, 1);

    let hit = db.query("SELECT * FROM orders WHERE order_id = 3", &[])?;
    assert_eq!(hit[0].get(1).unwrap(), &Value::String("shipped".to_string()));
    let other = db.query("SELECT * FROM orders WHERE order_id = 4", &[])?;
    assert_eq!(other[0].get(1).unwrap(), &Value::String("open".to_string()));
    Ok(())
}

#[test]
fn update_set_literal_containing_where_and_keywords() -> Result<()> {
    // The SET literal contains 'where', 'or', 'and' — the WHERE-splitter and
    // bail checks must skip string literals.
    let db = create_test_db()?;
    db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")?;
    db.execute("INSERT INTO notes (id, body) VALUES (1, 'x')")?;
    db.execute("INSERT INTO notes (id, body) VALUES (2, 'y')")?;

    let n = db.execute("UPDATE notes SET body = 'tell me where or and select' WHERE id = 1")?;
    assert_eq!(n, 1);

    let hit = db.query("SELECT * FROM notes WHERE id = 1", &[])?;
    assert_eq!(
        hit[0].get(1).unwrap(),
        &Value::String("tell me where or and select".to_string())
    );
    let other = db.query("SELECT * FROM notes WHERE id = 2", &[])?;
    assert_eq!(other[0].get(1).unwrap(), &Value::String("y".to_string()));
    Ok(())
}

#[test]
fn delete_where_finalized_eq_literal() -> Result<()> {
    // "finalized" contains the substring IN — pre-R1.2 this bailed the fast
    // delete. PK variant (fast-path eligible after R1.2):
    let db = create_test_db()?;
    db.execute("CREATE TABLE jobs (finalized INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO jobs (finalized, name) VALUES (1, 'a')")?;
    db.execute("INSERT INTO jobs (finalized, name) VALUES (2, 'b')")?;

    let n = db.execute("DELETE FROM jobs WHERE finalized = 1")?;
    assert_eq!(n, 1);
    let rest = db.query("SELECT * FROM jobs", &[])?;
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].get(0).unwrap(), &Value::Int4(2));

    // Non-PK variant must still execute correctly (falls back to the planner
    // because `finalized` is not the PK there — graceful, just not fast).
    db.execute("CREATE TABLE tasks (id INT PRIMARY KEY, finalized INT)")?;
    db.execute("INSERT INTO tasks (id, finalized) VALUES (10, 1)")?;
    db.execute("INSERT INTO tasks (id, finalized) VALUES (11, 0)")?;
    db.execute("INSERT INTO tasks (id, finalized) VALUES (12, 1)")?;
    let n = db.execute("DELETE FROM tasks WHERE finalized = 1")?;
    assert_eq!(n, 2);
    let rest = db.query("SELECT * FROM tasks", &[])?;
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].get(0).unwrap(), &Value::Int4(11));
    Ok(())
}

#[test]
fn select_where_points_eq_literal_and_param() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE scores (points INT PRIMARY KEY, val TEXT)")?;
    db.execute("INSERT INTO scores (points, val) VALUES (5, 'a')")?;
    db.execute("INSERT INTO scores (points, val) VALUES (6, 'b')")?;

    // Literal form (fast_select_lookup).
    let rows = db.query("SELECT * FROM scores WHERE points = 5", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(5));
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("a".to_string()));

    // Parameterized form (try_fast_select_params).
    let rows = db.query_params("SELECT * FROM scores WHERE points = $1", &[Value::Int4(5)])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("a".to_string()));

    // Miss must return empty, not error.
    let rows = db.query("SELECT * FROM scores WHERE points = 99", &[])?;
    assert!(rows.is_empty());
    Ok(())
}

#[test]
fn select_where_text_pk_literal_contains_keywords() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE kv (name TEXT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO kv (name, v) VALUES ('no limit or order', 7)")?;

    let rows = db.query("SELECT * FROM kv WHERE name = 'no limit or order'", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::Int4(7));
    Ok(())
}

// ============================================================================
// Negative matrix: real keywords MUST still bail to the planner and execute
// correctly there. If a fast path wrongly claimed any of these, the row
// assertions below would fail.
// ============================================================================

#[test]
fn insert_select_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, v TEXT)")?;
    db.execute("CREATE TABLE dst (id INT PRIMARY KEY, v TEXT)")?;
    db.execute("INSERT INTO src (id, v) VALUES (1, 'a')")?;
    db.execute("INSERT INTO src (id, v) VALUES (2, 'b')")?;

    let n = db.execute("INSERT INTO dst SELECT * FROM src")?;
    assert_eq!(n, 2);
    let rows = db.query("SELECT * FROM dst WHERE id = 2", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("b".to_string()));
    Ok(())
}

#[test]
fn insert_default_keyword_in_values_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE cfg (id INT PRIMARY KEY, n INT DEFAULT 42)")?;

    let n = db.execute("INSERT INTO cfg (id, n) VALUES (1, DEFAULT)")?;
    assert_eq!(n, 1);
    let rows = db.query("SELECT * FROM cfg WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get(1).unwrap(),
        &Value::Int4(42),
        "DEFAULT must materialize to 42"
    );
    Ok(())
}

#[test]
fn update_with_and_conjunction_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE pairs (id INT PRIMARY KEY, a INT, b INT, v INT)")?;
    db.execute("INSERT INTO pairs (id, a, b, v) VALUES (1, 1, 2, 0)")?;
    db.execute("INSERT INTO pairs (id, a, b, v) VALUES (2, 1, 3, 0)")?;

    // WHERE a = 1 AND b = 2 — both predicates must apply.
    let n = db.execute("UPDATE pairs SET v = 9 WHERE a = 1 AND b = 2")?;
    assert_eq!(n, 1);
    let r1 = db.query("SELECT * FROM pairs WHERE id = 1", &[])?;
    assert_eq!(r1[0].get(3).unwrap(), &Value::Int4(9));
    let r2 = db.query("SELECT * FROM pairs WHERE id = 2", &[])?;
    assert_eq!(r2[0].get(3).unwrap(), &Value::Int4(0), "AND row 2 must NOT be updated");
    Ok(())
}

#[test]
fn update_where_in_list_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE bulk (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO bulk (id, v) VALUES (1, 0)")?;
    db.execute("INSERT INTO bulk (id, v) VALUES (2, 0)")?;
    db.execute("INSERT INTO bulk (id, v) VALUES (3, 0)")?;

    let n = db.execute("UPDATE bulk SET v = 7 WHERE id IN (1, 2)")?;
    assert_eq!(n, 2);
    for (id, expected) in [(1, 7), (2, 7), (3, 0)] {
        let rows = db.query(&format!("SELECT * FROM bulk WHERE id = {id}"), &[])?;
        assert_eq!(rows[0].get(1).unwrap(), &Value::Int4(expected), "row {id}");
    }
    Ok(())
}

#[test]
fn delete_with_or_disjunction_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE dl (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO dl (id, v) VALUES (1, 0)")?;
    db.execute("INSERT INTO dl (id, v) VALUES (2, 0)")?;
    db.execute("INSERT INTO dl (id, v) VALUES (3, 0)")?;

    let n = db.execute("DELETE FROM dl WHERE id = 1 OR id = 2")?;
    assert_eq!(n, 2);
    let rest = db.query("SELECT * FROM dl", &[])?;
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].get(0).unwrap(), &Value::Int4(3));
    Ok(())
}

#[test]
fn select_with_order_by_and_limit_still_routed_to_planner() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE sl (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO sl (id, v) VALUES (1, 30)")?;
    db.execute("INSERT INTO sl (id, v) VALUES (2, 10)")?;
    db.execute("INSERT INTO sl (id, v) VALUES (3, 20)")?;

    // ORDER must still bail the fast select (word-bounded keyword after the
    // predicate) and the planner must apply the ordering.
    let rows = db.query("SELECT * FROM sl WHERE id = 1 ORDER BY id", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::Int4(30));

    // LIMIT must still bail and be honored.
    let rows = db.query("SELECT * FROM sl WHERE id = 2 LIMIT 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::Int4(10));
    Ok(())
}

// ============================================================================
// Dual-database equivalence: a mixed workload of newly-eligible statements
// must leave the database in exactly the same state as a database whose
// equivalent end state is built from unambiguous plan-path statements.
// ============================================================================

#[test]
fn mixed_workload_end_state_equivalence() -> Result<()> {
    let fast = create_test_db()?;
    let reference = create_test_db()?;
    for db in [&fast, &reference] {
        db.execute("CREATE TABLE inventory (order_id INT PRIMARY KEY, points INT, note TEXT)")?;
    }

    // Workload on `fast`: every statement is a pre-R1.2 false-positive shape.
    fast.execute("INSERT INTO inventory (order_id, points, note) VALUES (1, 5, 'select default')")?;
    fast.execute("INSERT INTO inventory (order_id, points, note) VALUES (2, 6, 'it''s in order')")?;
    fast.execute("INSERT INTO inventory (order_id, points, note) VALUES (3, 7, 'plain')")?;
    fast.execute("UPDATE inventory SET note = 'or and in between' WHERE order_id = 1")?;
    fast.execute("UPDATE inventory SET points = 99 WHERE order_id = 2")?;
    fast.execute("DELETE FROM inventory WHERE order_id = 3")?;

    // Equivalent end state on `reference`, built with statements that are
    // unambiguously planner-routed (multi-predicate WHERE / IN lists).
    reference.execute("INSERT INTO inventory (order_id, points, note) VALUES (1, 5, 'select default')")?;
    reference.execute("INSERT INTO inventory (order_id, points, note) VALUES (2, 6, 'it''s in order')")?;
    reference.execute("INSERT INTO inventory (order_id, points, note) VALUES (3, 7, 'plain')")?;
    reference.execute("UPDATE inventory SET note = 'or and in between' WHERE order_id = 1 AND points = 5")?;
    reference.execute("UPDATE inventory SET points = 99 WHERE order_id IN (2)")?;
    reference.execute("DELETE FROM inventory WHERE order_id = 3 OR order_id = 3")?;

    for id in 1..=3 {
        let a = fast.query(&format!("SELECT * FROM inventory WHERE order_id = {id}"), &[])?;
        let b = reference.query(
            &format!("SELECT order_id, points, note FROM inventory WHERE order_id = {id} AND order_id = {id}"),
            &[],
        )?;
        assert_eq!(a.len(), b.len(), "row count mismatch for order_id={id}");
        for (ta, tb) in a.iter().zip(b.iter()) {
            for col in 0..3 {
                assert_eq!(ta.get(col), tb.get(col), "order_id={id} col={col}");
            }
        }
    }
    Ok(())
}

// ============================================================================
// W1.3: generation-stamped catalog-existence cache. The indexed point/IN/range
// fast paths now classify a table via `StorageEngine::cached_table_kind`
// (generation-stamped) instead of two RocksDB metadata point-gets per probe.
// These are end-to-end correctness guards: an existence-changing DDL or branch
// switch must invalidate the cache so a probe never serves a stale
// classification (which would fast-path a dropped/replaced table).
// ============================================================================

#[test]
fn dropped_table_point_requery_is_clean_not_stale() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE cache_probe (id INT PRIMARY KEY, v TEXT)")?;
    db.execute("INSERT INTO cache_probe (id, v) VALUES (1, 'a')")?;

    // Prime the indexed point-read fast path (populates the existence cache
    // entry for `cache_probe` as a regular Table).
    let rows = db.query("SELECT * FROM cache_probe WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("a".to_string()));

    // DROP bumps the schema generation; the immediate re-query of the same
    // indexed point-read shape must NOT serve a stale fast-path hit.
    db.execute("DROP TABLE cache_probe")?;
    let requery = db.query("SELECT * FROM cache_probe WHERE id = 1", &[]);
    assert!(
        requery.is_err(),
        "querying a dropped table must error, not return a stale fast-path row"
    );
    Ok(())
}

#[test]
fn dropped_table_range_requery_is_clean_not_stale() -> Result<()> {
    // Exercises the third probe site (indexed range scan fast path).
    let db = create_test_db()?;
    db.execute("CREATE TABLE range_probe (id INT PRIMARY KEY, v TEXT)")?;
    for id in 1..=5 {
        db.execute(&format!("INSERT INTO range_probe (id, v) VALUES ({id}, 'r{id}')"))?;
    }

    // Prime the range fast path.
    let rows = db.query("SELECT * FROM range_probe WHERE id >= 2 AND id <= 4", &[])?;
    assert_eq!(rows.len(), 3);

    db.execute("DROP TABLE range_probe")?;
    let requery = db.query("SELECT * FROM range_probe WHERE id >= 2 AND id <= 4", &[]);
    assert!(
        requery.is_err(),
        "range query on a dropped table must error, not return stale rows"
    );
    Ok(())
}

#[test]
fn recreated_table_after_drop_reads_fresh_rows() -> Result<()> {
    // DROP + CREATE the same name: the fast path must read the NEW table's
    // rows, never a stale classification pointing at the old table.
    let db = create_test_db()?;
    db.execute("CREATE TABLE rc (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO rc (id, v) VALUES (1, 100)")?;
    assert_eq!(
        db.query("SELECT * FROM rc WHERE id = 1", &[])?[0].get(1).unwrap(),
        &Value::Int4(100)
    );

    db.execute("DROP TABLE rc")?;
    db.execute("CREATE TABLE rc (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO rc (id, v) VALUES (1, 200)")?;

    let rows = db.query("SELECT * FROM rc WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get(1).unwrap(),
        &Value::Int4(200),
        "re-created table must read fresh rows, not a stale cached probe"
    );
    Ok(())
}

#[test]
fn matview_probe_after_create_honors_mv_semantics() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE base (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO base (id, v) VALUES (1, 10)")?;
    db.execute("INSERT INTO base (id, v) VALUES (2, 20)")?;

    // CREATE MATERIALIZED VIEW bumps the generation; a subsequent probe of the
    // view name must classify it as a materialized view (bail the plain-table
    // fast path) and resolve via MV semantics.
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM base")?;
    let rows = db.query("SELECT * FROM mv WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1).unwrap(), &Value::Int4(10));
    Ok(())
}

#[test]
fn indexed_point_read_correct_across_branch_switch() -> Result<()> {
    // A branch switch changes catalog/data visibility without table DDL. The
    // existence cache must not leak wrong-branch classification: after a switch
    // the point-read fast path must serve the active branch's rows.
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.wal_enabled = false;
    let db = EmbeddedDatabase::with_config(config).expect("create in-memory db");

    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)")?;
    db.execute("INSERT INTO acct (id, bal) VALUES (1, 100)")?;
    // Prime the fast path + existence cache on main.
    assert_eq!(
        db.query("SELECT * FROM acct WHERE id = 1", &[])?[0].get(1).unwrap(),
        &Value::Int4(100)
    );

    db.execute("CREATE BRANCH b1 AS OF NOW")?;
    db.execute("USE BRANCH b1")?;
    db.execute("UPDATE acct SET bal = 999 WHERE id = 1")?;
    assert_eq!(
        db.query("SELECT * FROM acct WHERE id = 1", &[])?[0].get(1).unwrap(),
        &Value::Int4(999),
        "branch must see its own update"
    );

    db.execute("USE BRANCH main")?;
    let rows = db.query("SELECT * FROM acct WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get(1).unwrap(),
        &Value::Int4(100),
        "main must read its own row after branch switch, not the branch's value"
    );
    Ok(())
}
