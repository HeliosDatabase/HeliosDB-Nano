//! `GENERATED ALWAYS AS (expr) STORED`.
//!
//! Before: the clause was accepted and the column stored as a plain column. An
//! INSERT that left it out stored NULL, an INSERT or UPDATE could write any value
//! into it, and an UPDATE of a column it reads left it stale. (`AS (expr) VIRTUAL`
//! was read as an identity column.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;
use heliosdb_nano::{EmbeddedDatabase, Value};

const LINES: &[&str] = &[
    "CREATE TABLE lines (id INT PRIMARY KEY, qty INT, price INT, label TEXT, \
     total INT GENERATED ALWAYS AS (qty * price) STORED, \
     shout TEXT GENERATED ALWAYS AS (upper(label)) STORED)",
];

const ALL: &str = "SELECT id, qty, price, total, shout FROM lines ORDER BY id";

#[test]
fn the_value_is_computed_on_insert() {
    for family in FAMILIES {
        let db = setup(LINES);
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a')",
        );
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (2, 3, 5, 'b'), (3, NULL, 5, NULL)",
        );
        // Any column order, and the explicit DEFAULT.
        ok(
            &db,
            family,
            "INSERT INTO lines (label, price, qty, id) VALUES ('d', 7, 6, 4)",
        );
        ok(&db, family, "INSERT INTO lines VALUES (5, 1, 1, 'e', DEFAULT, DEFAULT)");
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label, total) VALUES (6, 2, 2, 'f', DEFAULT)",
        );
        db.execute_params(
            "INSERT INTO lines (id, qty, price, label) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int4(7),
                Value::Int4(4),
                Value::Int4(25),
                Value::String("g".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            text_rows(&db, ALL),
            vec![
                vec!["1", "2", "10", "20", "A"],
                vec!["2", "3", "5", "15", "B"],
                vec!["3", "NULL", "5", "NULL", "NULL"],
                vec!["4", "6", "7", "42", "D"],
                vec!["5", "1", "1", "1", "E"],
                vec!["6", "2", "2", "4", "F"],
                vec!["7", "4", "25", "100", "G"],
            ]
        );
    }
}

#[test]
fn the_value_follows_an_update_of_a_column_it_reads() {
    for family in FAMILIES {
        let db = setup(LINES);
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a'), (2, 3, 5, 'b')",
        );
        ok(&db, family, "UPDATE lines SET qty = 4 WHERE id = 1");
        ok(
            &db,
            family,
            "UPDATE lines SET price = price + 1, label = 'bee' WHERE id = 2",
        );
        // A column no generated column reads changes nothing else.
        ok(&db, family, "UPDATE lines SET id = 20 WHERE id = 2");
        db.execute_params(
            "UPDATE lines SET qty = $1 WHERE id = $2",
            &[Value::Int4(9), Value::Int4(20)],
        )
        .unwrap();
        ok(&db, family, "UPDATE lines SET total = DEFAULT WHERE id = 1");
        assert_eq!(
            text_rows(&db, ALL),
            vec![vec!["1", "4", "10", "40", "A"], vec!["20", "9", "6", "54", "BEE"]]
        );
        // An upsert that updates.
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (1, 5, 5, 'z') \
             ON CONFLICT (id) DO UPDATE SET qty = excluded.qty, label = excluded.label",
        );
        assert_eq!(
            text_rows(&db, "SELECT qty, price, total, shout FROM lines WHERE id = 1"),
            vec![vec!["5", "10", "50", "Z"]]
        );
    }
}

#[test]
fn a_direct_write_is_refused() {
    for family in FAMILIES {
        let db = setup(LINES);
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a')",
        );
        for statement in [
            "INSERT INTO lines (id, qty, price, label, total) VALUES (2, 2, 10, 'a', 20)",
            "INSERT INTO lines VALUES (2, 2, 10, 'a', 20, 'A')",
            "UPDATE lines SET total = 5 WHERE id = 1",
            "UPDATE lines SET qty = 1, shout = 'x' WHERE id = 1",
            "INSERT INTO lines (id, qty, price, label, total) SELECT 3, 1, 1, 'x', 1",
        ] {
            let message = refused(&db, family, statement);
            assert!(message.contains("generated column"), "{statement}: {message}");
        }
        assert_eq!(text_rows(&db, ALL), vec![vec!["1", "2", "10", "20", "A"]]);
    }
}

#[test]
fn insert_select_computes_the_value() {
    for family in FAMILIES {
        let db = setup(LINES);
        ok(
            &db,
            family,
            "CREATE TABLE source (id INT PRIMARY KEY, n INT, p INT, name TEXT)",
        );
        ok(&db, family, "INSERT INTO source VALUES (1, 2, 3, 'x'), (2, 4, 5, 'y')");
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) SELECT id, n AS qty, p AS price, name AS label FROM source",
        );
        assert_eq!(
            text_rows(&db, ALL),
            vec![vec!["1", "2", "3", "6", "X"], vec!["2", "4", "5", "20", "Y"]]
        );
    }
}

#[test]
fn expressions_postgresql_refuses_are_refused() {
    for family in FAMILIES {
        let db = setup(&["CREATE TABLE other (id INT PRIMARY KEY)"]);
        for (statement, why) in [
            (
                "CREATE TABLE g1 (a INT, b INT GENERATED ALWAYS AS ((SELECT max(id) FROM other)) STORED)",
                "subquery",
            ),
            (
                "CREATE TABLE g2 (a INT, b TIMESTAMP GENERATED ALWAYS AS (now()) STORED)",
                "not immutable",
            ),
            (
                "CREATE TABLE g3 (a INT, b DOUBLE PRECISION GENERATED ALWAYS AS (random()) STORED)",
                "not immutable",
            ),
            (
                "CREATE TABLE g4 (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED, \
                 c INT GENERATED ALWAYS AS (b + 1) STORED)",
                "generated column",
            ),
            (
                "CREATE TABLE g5 (a INT, b INT GENERATED ALWAYS AS (nope + 1) STORED)",
                "nope",
            ),
        ] {
            let message = refused(&db, family, statement);
            // A subquery may be refused before the generation rule sees it.
            if why != "subquery" {
                assert!(message.contains("generation expression"), "{statement}: {message}");
                assert!(message.contains(why), "{statement}: {message}");
            }
        }
        // Nothing was created.
        assert!(text_rows(
            &db,
            "SELECT table_name FROM information_schema.tables WHERE table_name LIKE 'g_'"
        )
        .is_empty());

        // VIRTUAL is refused: the value would have to be computed by every reader.
        let message = refused(
            &db,
            family,
            "CREATE TABLE v1 (a INT, b INT GENERATED ALWAYS AS (a + 1) VIRTUAL)",
        );
        assert!(
            message.contains("VIRTUAL") && message.contains("is not supported"),
            "{message}"
        );
        let message = refused(
            &db,
            family,
            "CREATE TABLE v2 (a INT, b INT GENERATED ALWAYS AS (a + 1))",
        );
        assert!(
            message.contains("VIRTUAL") && message.contains("is not supported"),
            "{message}"
        );
    }
}

#[test]
fn add_column_fills_the_rows_that_exist() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE boxes (id INT PRIMARY KEY, w INT, h INT)",
            "INSERT INTO boxes VALUES (1, 2, 3), (2, 4, 5), (3, NULL, 5)",
        ]);
        ok(
            &db,
            family,
            "ALTER TABLE boxes ADD COLUMN area INT GENERATED ALWAYS AS (w * h) STORED",
        );
        assert_eq!(
            text_rows(&db, "SELECT id, area FROM boxes ORDER BY id"),
            vec![vec!["1", "6"], vec!["2", "20"], vec!["3", "NULL"]]
        );
        ok(&db, family, "INSERT INTO boxes (id, w, h) VALUES (4, 6, 7)");
        ok(&db, family, "UPDATE boxes SET w = 10 WHERE id = 1");
        assert_eq!(
            text_rows(&db, "SELECT id, area FROM boxes WHERE id IN (1, 4) ORDER BY id"),
            vec![vec!["1", "30"], vec!["4", "42"]]
        );
        let message = refused(&db, family, "UPDATE boxes SET area = 1 WHERE id = 1");
        assert!(message.contains("generated column"), "{message}");

        // A column it reads cannot go or be renamed under it; the generated column can go.
        let message = refused(&db, family, "ALTER TABLE boxes DROP COLUMN w");
        assert!(message.contains("area"), "{message}");
        let message = refused(&db, family, "ALTER TABLE boxes RENAME COLUMN h TO height");
        assert!(message.contains("area"), "{message}");
        ok(&db, family, "ALTER TABLE boxes DROP COLUMN area");
        ok(&db, family, "ALTER TABLE boxes DROP COLUMN w");
        ok(&db, family, "INSERT INTO boxes (id, h) VALUES (9, 1)");
    }
}

#[test]
fn it_is_indexed_like_any_column() {
    for family in FAMILIES {
        let db = setup(LINES);
        ok(&db, family, "CREATE INDEX lines_total_idx ON lines (total)");
        ok(&db, family, "CREATE UNIQUE INDEX lines_shout_uq ON lines (shout)");
        ok(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a'), (2, 4, 5, 'b')",
        );
        assert_eq!(
            text_rows(&db, "SELECT id FROM lines WHERE total = 20 ORDER BY id"),
            vec![vec!["1"], vec!["2"]]
        );
        ok(&db, family, "UPDATE lines SET qty = 3 WHERE id = 1");
        assert_eq!(text_rows(&db, "SELECT id FROM lines WHERE total = 20"), vec![vec!["2"]]);
        assert_eq!(text_rows(&db, "SELECT id FROM lines WHERE total = 30"), vec![vec!["1"]]);
        // The unique index over the generated value refuses what computes to a duplicate.
        let message = refused(
            &db,
            family,
            "INSERT INTO lines (id, qty, price, label) VALUES (3, 1, 1, 'A')",
        );
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        assert_eq!(
            text_rows(&db, "SELECT id FROM lines WHERE shout = 'B'"),
            vec![vec!["2"]]
        );
    }
}

#[test]
fn the_catalog_shows_it() {
    let db = setup(LINES);
    let columns = text_rows(
        &db,
        "SELECT column_name, is_generated, generation_expression, column_default FROM information_schema.columns \
         WHERE table_name = 'lines' AND column_name IN ('qty', 'total') ORDER BY column_name",
    );
    assert_eq!(columns[0], vec!["qty", "NEVER", "NULL", "NULL"]);
    assert_eq!(
        (columns[1][0].as_str(), columns[1][1].as_str(), columns[1][3].as_str()),
        ("total", "ALWAYS", "NULL")
    );
    assert!(
        columns[1][2].contains("qty") && columns[1][2].contains("price"),
        "{columns:?}"
    );
    assert_eq!(
        text_rows(
            &db,
            "SELECT attname, attgenerated FROM pg_attribute WHERE attname IN ('qty', 'total', 'shout') ORDER BY attname"
        ),
        vec![vec!["qty", ""], vec!["shout", "s"], vec!["total", "s"]]
    );
    // The column's definition says it: no separate constraint is listed.
    assert!(text_rows(
        &db,
        "SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'lines' AND \
         constraint_type = 'CHECK'"
    )
    .is_empty());
    assert!(text_rows(&db, "SELECT constraint_name FROM information_schema.check_constraints").is_empty());
}

#[test]
fn it_survives_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = open(dir.path());
        for statement in LINES {
            db.execute(statement).unwrap();
        }
        db.execute("INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a')")
            .unwrap();
        db.close().unwrap();
    }
    let db = open(dir.path());
    db.execute("INSERT INTO lines (id, qty, price, label) VALUES (2, 3, 3, 'b')")
        .unwrap();
    db.execute_params("UPDATE lines SET price = 11 WHERE id = 1", &[])
        .unwrap();
    assert_eq!(
        text_rows(&db, ALL),
        vec![vec!["1", "2", "11", "22", "A"], vec!["2", "3", "3", "9", "B"]]
    );
    let message = refused(&db, Family::Text, "UPDATE lines SET total = 0");
    assert!(message.contains("generated column"), "{message}");
}

#[derive(Clone, Copy, Debug)]
enum Format {
    Sql,
    Binary,
}

#[test]
fn a_dump_carries_it_in_both_formats() {
    for format in [Format::Sql, Format::Binary] {
        let source = setup(LINES);
        source
            .execute("INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a'), (2, 3, 5, 'b')")
            .unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let mut restored = EmbeddedDatabase::new_in_memory().unwrap();
        match format {
            Format::Sql => {
                let path = dir.path().join("dump.sql");
                source.dump_sql(&path).unwrap();
                let script = std::fs::read_to_string(&path).unwrap();
                assert!(script.contains("GENERATED ALWAYS AS"), "{script}");
                assert!(!script.contains("helios_generated_stored"), "{script}");
                restored
                    .execute_sql_script(&script)
                    .unwrap_or_else(|e| panic!("sql restore failed: {e}\n----\n{script}"));
            }
            Format::Binary => {
                let path = dir.path().join("dump.heliodump");
                source.dump_full(&path).unwrap();
                restored.restore_from_dump(&path).unwrap();
            }
        }
        assert_eq!(
            text_rows(&restored, ALL),
            vec![vec!["1", "2", "10", "20", "A"], vec!["2", "3", "5", "15", "B"]],
            "{format:?}"
        );
        // It is still a generated column after the restore.
        restored
            .execute("INSERT INTO lines (id, qty, price, label) VALUES (3, 4, 4, 'c')")
            .unwrap();
        restored.execute("UPDATE lines SET qty = 10 WHERE id = 1").unwrap();
        assert_eq!(
            text_rows(
                &restored,
                "SELECT id, total, shout FROM lines WHERE id IN (1, 3) ORDER BY id"
            ),
            vec![vec!["1", "100", "A"], vec!["3", "16", "C"]],
            "{format:?}"
        );
        let message = refused(&restored, Family::Text, "UPDATE lines SET total = 0");
        assert!(message.contains("generated column"), "{format:?}: {message}");
    }
}

#[tokio::test]
async fn over_the_wire() {
    let server = serve(setup(LINES)).await;
    let (client, task) = connect(&server).await;
    wire_ok(
        &client,
        "INSERT INTO lines (id, qty, price, label) VALUES (1, 2, 10, 'a')",
    )
    .await;
    client
        .execute(
            "INSERT INTO lines (id, qty, price, label) VALUES ($1, $2, $3, $4)",
            &[&2i32, &3i32, &5i32, &"b"],
        )
        .await
        .unwrap();
    client
        .execute("UPDATE lines SET qty = $1 WHERE id = $2", &[&7i32, &1i32])
        .await
        .unwrap();
    assert_eq!(
        wire_rows(&client, ALL).await,
        vec![vec!["1", "7", "10", "70", "A"], vec!["2", "3", "5", "15", "B"]]
    );
    let (code, message) = wire_refused(&client, "UPDATE lines SET total = 1 WHERE id = 1").await;
    assert_eq!(code, "428C9", "{message}");
    let (code, message) = wire_refused(
        &client,
        "INSERT INTO lines (id, qty, price, label, total) VALUES (3, 1, 1, 'c', 1)",
    )
    .await;
    assert_eq!(code, "428C9", "{message}");
    let error = client
        .execute("UPDATE lines SET shout = $1 WHERE id = $2", &[&"x", &1i32])
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "428C9", "{error}");
    let (code, message) = wire_refused(
        &client,
        "CREATE TABLE bad (a INT, b TIMESTAMP GENERATED ALWAYS AS (now()) STORED)",
    )
    .await;
    assert_eq!(code, "42P17", "{message}");
    let (code, message) = wire_refused(
        &client,
        "CREATE TABLE virt (a INT, b INT GENERATED ALWAYS AS (a + 1) VIRTUAL)",
    )
    .await;
    assert_eq!(code, "0A000", "{message}");
    task.abort();
}
