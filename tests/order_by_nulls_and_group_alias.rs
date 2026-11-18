//! `ORDER BY … NULLS FIRST / LAST` is obeyed, and a grouped query can be
//! ordered by an output alias together with a group key.
//!
//! GitHub #43: the parser read the clause and the planner dropped it, so
//! `ORDER BY due ASC NULLS LAST` returned the NULLs first. The engine's own
//! order (NULL below every value: first under ASC, last under DESC) is
//! unchanged; an explicit clause that asks for the other placement now gets it.
//!
//! GitHub #44: `GROUP BY s.id, s.name ORDER BY app_count DESC, s.name` failed
//! with "Column 'group_1' not found in schema": the alias exists above the
//! projection, the group key below it, and the sort was placed where one of
//! the two was missing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Int2(n) => Some(i64::from(*n)),
        Value::Int4(n) => Some(i64::from(*n)),
        Value::Int8(n) => Some(*n),
        other => panic!("not an integer: {other:?}"),
    }
}

/// First column of every row, through both statement families.
fn column(db: &EmbeddedDatabase, sql: &str) -> Vec<Option<i64>> {
    let read =
        |rows: Vec<heliosdb_nano::Tuple>| -> Vec<Option<i64>> { rows.iter().map(|row| int(&row.values[0])).collect() };
    let text = read(db.query(sql, &[]).unwrap_or_else(|error| panic!("{sql}: {error}")));
    let (rows, _) = db
        .query_with_columns(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(text, read(rows), "{sql}: the two read entry points differ");
    text
}

/// `t(id, v, g)`: v = NULL, 20, 10, NULL, 30.
fn nulls() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT, g INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, NULL, 1), (2, 20, 1), (3, 10, 2), (4, NULL, 2), (5, 30, 2)")
        .unwrap();
    db
}

const N: Option<i64> = None;

#[test]
fn an_explicit_null_placement_is_obeyed_in_both_directions() {
    let db = nulls();
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v ASC NULLS LAST"),
        [Some(10), Some(20), Some(30), N, N]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v NULLS LAST"),
        [Some(10), Some(20), Some(30), N, N]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v DESC NULLS FIRST"),
        [N, N, Some(30), Some(20), Some(10)]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v ASC NULLS FIRST"),
        [N, N, Some(10), Some(20), Some(30)]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v DESC NULLS LAST"),
        [Some(30), Some(20), Some(10), N, N]
    );
}

#[test]
fn without_the_clause_the_order_is_what_it_was() {
    let db = nulls();
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v"),
        [N, N, Some(10), Some(20), Some(30)]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v DESC"),
        [Some(30), Some(20), Some(10), N, N]
    );
}

#[test]
fn the_placement_holds_for_later_keys_ordinals_aliases_and_limits() {
    let db = nulls();
    // Ties among the NULLs are broken by the next key.
    assert_eq!(
        column(&db, "SELECT id FROM t ORDER BY v ASC NULLS LAST, id DESC"),
        [Some(3), Some(2), Some(5), Some(4), Some(1)]
    );
    // A second key with its own placement.
    assert_eq!(
        column(&db, "SELECT id FROM t ORDER BY g, v DESC NULLS FIRST"),
        [Some(1), Some(2), Some(4), Some(5), Some(3)]
    );
    // By position and by alias.
    assert_eq!(
        column(&db, "SELECT v, id FROM t ORDER BY 1 NULLS LAST, 2"),
        [Some(10), Some(20), Some(30), N, N]
    );
    assert_eq!(
        column(&db, "SELECT v AS due FROM t ORDER BY due NULLS LAST"),
        [Some(10), Some(20), Some(30), N, N]
    );
    // A column the select list does not carry.
    assert_eq!(
        column(&db, "SELECT id FROM t ORDER BY v NULLS LAST, id"),
        [Some(3), Some(2), Some(5), Some(1), Some(4)]
    );
    // Under a LIMIT (the top-k route) and with an index on the key.
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v ASC NULLS LAST LIMIT 2"),
        [Some(10), Some(20)]
    );
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v DESC NULLS FIRST LIMIT 3"),
        [N, N, Some(30)]
    );
    db.execute("CREATE INDEX t_v ON t (v)").unwrap();
    assert_eq!(
        column(&db, "SELECT v FROM t ORDER BY v ASC NULLS LAST LIMIT 4"),
        [Some(10), Some(20), Some(30), N]
    );
    assert_eq!(column(&db, "SELECT v FROM t ORDER BY v LIMIT 3"), [N, N, Some(10)]);
}

#[test]
fn the_placement_holds_over_a_group_by_a_union_and_inside_a_window() {
    let db = nulls();
    assert_eq!(
        column(&db, "SELECT v FROM t GROUP BY v ORDER BY v NULLS LAST"),
        [Some(10), Some(20), Some(30), N]
    );
    assert_eq!(
        column(
            &db,
            "SELECT MAX(v) AS top FROM t GROUP BY id ORDER BY top DESC NULLS FIRST, id"
        ),
        [N, N, Some(30), Some(20), Some(10)]
    );
    assert_eq!(
        column(
            &db,
            "SELECT v FROM t WHERE id <= 2 UNION ALL SELECT v FROM t WHERE id > 2 ORDER BY v NULLS LAST"
        ),
        [Some(10), Some(20), Some(30), N, N]
    );
    // ROW_NUMBER counts along the window's order: values first, then the NULLs.
    assert_eq!(
        column(
            &db,
            "SELECT ROW_NUMBER() OVER (ORDER BY v ASC NULLS LAST, id) FROM t ORDER BY id"
        ),
        [Some(4), Some(2), Some(1), Some(5), Some(3)]
    );
}

/// The schema of GitHub #44.
fn streams() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE streams (id SERIAL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    db.execute("CREATE TABLE stream_apps (id SERIAL PRIMARY KEY, stream_id INTEGER NOT NULL, app_id INTEGER NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO streams (name) VALUES ('audit'), ('checkout'), ('api')")
        .unwrap();
    db.execute("INSERT INTO stream_apps (stream_id, app_id) VALUES (1,1),(1,2),(1,3),(2,1),(3,3)")
        .unwrap();
    db
}

#[test]
fn a_grouped_query_is_ordered_by_an_alias_together_with_a_group_key() {
    let db = streams();
    const HEAD: &str = "SELECT s.id, s.name, COUNT(DISTINCT sa.app_id) AS app_count \
                        FROM streams s JOIN stream_apps sa ON sa.stream_id = s.id GROUP BY s.id, s.name";
    // audit has 3 apps; api and checkout have 1 each and are ordered by name.
    for order in [
        "ORDER BY app_count DESC, s.name ASC",
        "ORDER BY app_count DESC, name",
        "ORDER BY COUNT(DISTINCT sa.app_id) DESC, s.name",
        "ORDER BY app_count DESC NULLS LAST, s.name ASC NULLS LAST",
        "ORDER BY 3 DESC, s.name",
    ] {
        assert_eq!(
            column(&db, &format!("{HEAD} {order}")),
            [Some(1), Some(3), Some(2)],
            "{order}"
        );
    }
    assert_eq!(
        column(&db, &format!("{HEAD} ORDER BY s.name DESC, app_count")),
        [Some(2), Some(1), Some(3)]
    );
    // Each alone, as before.
    assert_eq!(
        column(&db, &format!("{HEAD} ORDER BY app_count, s.id")),
        [Some(2), Some(3), Some(1)]
    );
    // A group key the select list does not carry, beside an alias.
    assert_eq!(
        column(
            &db,
            "SELECT COUNT(*) AS n FROM stream_apps GROUP BY stream_id ORDER BY n DESC, stream_id DESC"
        ),
        [Some(3), Some(1), Some(1)]
    );
    assert_eq!(
        column(
            &db,
            "SELECT stream_id, COUNT(*) AS n FROM stream_apps GROUP BY stream_id HAVING COUNT(*) < 3 ORDER BY n, stream_id DESC"
        ),
        [Some(3), Some(2)]
    );
}
