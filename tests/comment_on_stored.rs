//! `COMMENT ON` stores the comment.
//!
//! Before: the statement was accepted and the text thrown away. `pg_description`
//! was always empty and there was no `obj_description()` / `col_description()`,
//! so `\d+` showed no description and a dump lost every comment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;
use heliosdb_nano::EmbeddedDatabase;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA app",
    "CREATE TABLE orders (id INT PRIMARY KEY, total NUMERIC(10,2), note TEXT)",
    "CREATE INDEX orders_note_idx ON orders (note)",
    "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
    "CREATE TABLE app.customers (id INT PRIMARY KEY, name TEXT)",
    "INSERT INTO orders VALUES (1, 10.50, 'first')",
];

const TABLE_COMMENT: &str = "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'orders'";
const TOTAL_COMMENT: &str = "SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a \
     JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname = 'orders' AND a.attname = 'total'";
const ALL_DESCRIPTIONS: &str = "SELECT description FROM pg_description ORDER BY description";

fn comment_everything(db: &EmbeddedDatabase, family: Family) {
    for statement in [
        "COMMENT ON TABLE orders IS 'placed orders'",
        "COMMENT ON COLUMN orders.total IS 'what the customer''s order came to'",
        "COMMENT ON COLUMN orders.note IS 'free text'",
        "COMMENT ON INDEX orders_note_idx IS 'lookup by note'",
        "COMMENT ON VIEW big_orders IS 'orders above 100'",
        "COMMENT ON SCHEMA app IS 'the application schema'",
        "COMMENT ON TABLE app.customers IS 'who buys'",
        "COMMENT ON CONSTRAINT orders_pkey ON orders IS 'the key'",
        "COMMENT ON FUNCTION some_function(integer) IS 'a function'",
    ] {
        ok(db, family, statement);
    }
}

#[test]
fn a_comment_is_read_back_from_the_catalog() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        assert!(text_rows(&db, ALL_DESCRIPTIONS).is_empty());
        assert_eq!(text_rows(&db, TABLE_COMMENT), vec![vec!["NULL"]]);
        comment_everything(&db, family);

        assert_eq!(text_rows(&db, TABLE_COMMENT), vec![vec!["placed orders"]]);
        assert_eq!(
            text_rows(&db, TOTAL_COMMENT),
            vec![vec!["what the customer's order came to"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(c.oid) FROM pg_class c WHERE c.relname = 'orders_note_idx'"
            ),
            vec![vec!["lookup by note"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'big_orders'"
            ),
            vec![vec!["orders above 100"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(n.oid, 'pg_namespace') FROM pg_namespace n WHERE n.nspname = 'app'"
            ),
            vec![vec!["the application schema"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'customers'"
            ),
            vec![vec!["who buys"]]
        );
        // The column comment belongs to its column: the table has its own.
        assert_eq!(
            text_rows(
                &db,
                "SELECT d.objsubid, d.description FROM pg_description d JOIN pg_class c ON c.oid = d.objoid \
                 WHERE c.relname = 'orders' ORDER BY d.objsubid"
            ),
            vec![
                vec!["0", "placed orders"],
                vec!["2", "what the customer's order came to"],
                vec!["3", "free text"]
            ]
        );
        let all: Vec<String> = text_rows(&db, ALL_DESCRIPTIONS)
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        for expected in [
            "lookup by note",
            "orders above 100",
            "the application schema",
            "who buys",
        ] {
            assert!(
                all.iter().any(|text| text == expected),
                "{expected} is missing from {all:?}"
            );
        }
    }
}

#[test]
fn a_comment_is_replaced_and_is_null_removes_it() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        comment_everything(&db, family);
        ok(&db, family, "COMMENT ON TABLE orders IS 'every order'");
        assert_eq!(text_rows(&db, TABLE_COMMENT), vec![vec!["every order"]]);
        ok(&db, family, "COMMENT ON TABLE orders IS NULL");
        assert_eq!(text_rows(&db, TABLE_COMMENT), vec![vec!["NULL"]]);
        ok(&db, family, "COMMENT ON COLUMN orders.total IS ''");
        assert_eq!(text_rows(&db, TOTAL_COMMENT), vec![vec!["NULL"]]);
        // Removing one that is not there is not an error.
        ok(&db, family, "COMMENT ON TABLE orders IS NULL");
        // The others are untouched.
        assert_eq!(
            text_rows(
                &db,
                "SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid = \
                 a.attrelid WHERE c.relname = 'orders' AND a.attname = 'note'"
            ),
            vec![vec!["free text"]]
        );
    }
}

#[test]
fn a_comment_follows_its_object_and_goes_with_it() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        comment_everything(&db, family);

        // The view reads the column about to be renamed: it goes first.
        ok(&db, family, "DROP VIEW big_orders");
        ok(&db, family, "ALTER TABLE orders RENAME COLUMN total TO amount");
        assert_eq!(
            text_rows(
                &db,
                "SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid = \
                 a.attrelid WHERE c.relname = 'orders' AND a.attname = 'amount'"
            ),
            vec![vec!["what the customer's order came to"]]
        );
        ok(&db, family, "ALTER TABLE orders DROP COLUMN note");
        // A new column of the dropped one's name has no comment.
        ok(&db, family, "ALTER TABLE orders ADD COLUMN note TEXT");
        assert_eq!(
            text_rows(
                &db,
                "SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid = \
                 a.attrelid WHERE c.relname = 'orders' AND a.attname = 'note'"
            ),
            vec![vec!["NULL"]]
        );

        ok(&db, family, "ALTER TABLE orders RENAME TO purchases");
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'purchases'"
            ),
            vec![vec!["placed orders"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid = \
                 a.attrelid WHERE c.relname = 'purchases' AND a.attname = 'amount'"
            ),
            vec![vec!["what the customer's order came to"]]
        );

        ok(&db, family, "DROP TABLE purchases");
        // A table made under the old name starts with no comment, on it or its columns.
        ok(&db, family, "CREATE TABLE purchases (id INT PRIMARY KEY, amount INT)");
        assert_eq!(
            text_rows(
                &db,
                "SELECT d.description FROM pg_description d JOIN pg_class c ON c.oid = d.objoid WHERE c.relname = \
                 'purchases'"
            ),
            Vec::<Vec<String>>::new()
        );
        // The view's comment went with the view.
        ok(&db, family, "CREATE VIEW big_orders AS SELECT id FROM purchases");
        assert_eq!(
            text_rows(
                &db,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'big_orders'"
            ),
            vec![vec!["NULL"]]
        );
    }
}

#[test]
fn comments_survive_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = open(dir.path());
        for statement in FIXTURE {
            db.execute(statement).unwrap();
        }
        comment_everything(&db, Family::Text);
        db.close().unwrap();
    }
    let db = open(dir.path());
    assert_eq!(text_rows(&db, TABLE_COMMENT), vec![vec!["placed orders"]]);
    assert_eq!(
        text_rows(&db, TOTAL_COMMENT),
        vec![vec!["what the customer's order came to"]]
    );
    assert_eq!(
        text_rows(
            &db,
            "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'big_orders'"
        ),
        vec![vec!["orders above 100"]]
    );
}

#[derive(Clone, Copy, Debug)]
enum Format {
    Sql,
    Binary,
}

#[test]
fn a_dump_carries_the_comments_in_both_formats() {
    for format in [Format::Sql, Format::Binary] {
        // One schema: what a dump does with other schemas is not this suite's.
        let source = setup(&[
            "CREATE TABLE orders (id INT PRIMARY KEY, total NUMERIC(10,2), note TEXT)",
            "CREATE INDEX orders_note_idx ON orders (note)",
            "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
            "INSERT INTO orders VALUES (1, 10.50, 'first')",
            "COMMENT ON TABLE orders IS 'placed orders'",
            "COMMENT ON COLUMN orders.total IS 'what the customer''s order came to'",
            "COMMENT ON INDEX orders_note_idx IS 'lookup by note'",
            "COMMENT ON VIEW big_orders IS 'orders above 100'",
            "COMMENT ON CONSTRAINT orders_pkey ON orders IS 'the key'",
            "COMMENT ON FUNCTION some_function(integer) IS 'a function'",
        ]);
        let dir = tempfile::TempDir::new().unwrap();
        let mut restored = EmbeddedDatabase::new_in_memory().unwrap();
        match format {
            Format::Sql => {
                let path = dir.path().join("dump.sql");
                source.dump_sql(&path).unwrap();
                let script = std::fs::read_to_string(&path).unwrap();
                assert!(
                    script.contains("COMMENT ON TABLE \"orders\" IS 'placed orders';"),
                    "{script}"
                );
                assert!(
                    script.contains("COMMENT ON FUNCTION \"some_function\" IS 'a function';"),
                    "{script}"
                );
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
            text_rows(&restored, TABLE_COMMENT),
            vec![vec!["placed orders"]],
            "{format:?}"
        );
        assert_eq!(
            text_rows(&restored, TOTAL_COMMENT),
            vec![vec!["what the customer's order came to"]],
            "{format:?}"
        );
        assert_eq!(
            text_rows(
                &restored,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.relname = 'big_orders'"
            ),
            vec![vec!["orders above 100"]],
            "{format:?}"
        );
        assert_eq!(
            text_rows(
                &restored,
                "SELECT obj_description(c.oid) FROM pg_class c WHERE c.relname = 'orders_note_idx'"
            ),
            vec![vec!["lookup by note"]],
            "{format:?}"
        );
        // A second dump of the restored database writes the same comments.
        assert_eq!(
            text_rows(&restored, ALL_DESCRIPTIONS),
            text_rows(&source, ALL_DESCRIPTIONS),
            "{format:?}"
        );
    }
}

#[tokio::test]
async fn over_the_wire() {
    let server = serve(setup(FIXTURE)).await;
    let (client, task) = connect(&server).await;
    wire_ok(&client, "COMMENT ON TABLE orders IS 'placed orders'").await;
    wire_ok(&client, "COMMENT ON COLUMN orders.total IS 'the total'").await;
    assert_eq!(wire_rows(&client, TABLE_COMMENT).await, vec![vec!["placed orders"]]);
    assert_eq!(wire_rows(&client, TOTAL_COMMENT).await, vec![vec!["the total"]]);
    // The extended protocol reads the same.
    let rows = client.query(TABLE_COMMENT, &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, Option<String>>(0).as_deref(), Some("placed orders"));
    wire_ok(&client, "COMMENT ON TABLE orders IS NULL").await;
    assert_eq!(wire_rows(&client, TABLE_COMMENT).await, vec![vec!["NULL"]]);
    let (code, message) = wire_refused(&client, "COMMENT ON TABLE no_such_table IS 'x'").await;
    assert_eq!(code, "42P01", "{message}");
    task.abort();
}
