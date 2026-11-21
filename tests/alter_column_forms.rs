//! `ALTER TABLE … ALTER COLUMN`: `SET DEFAULT` / `DROP DEFAULT`, `SET NOT NULL`,
//! `[SET DATA] TYPE … [USING …]`, `ADD GENERATED … AS IDENTITY`, `DROP IDENTITY`,
//! `SET STATISTICS`.
//!
//! Before: only `DROP NOT NULL` had a plan. Every other form answered
//! "Unsupported ALTER TABLE operation: …" with a debug rendering of the syntax
//! tree, as an internal error (XX000), and `DROP IDENTITY` / `SET STATISTICS` did
//! not parse.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;
use heliosdb_nano::Value;

const ITEMS: &[&str] = &[
    "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT, note TEXT)",
    "INSERT INTO items VALUES (1, 'bolt', 5, NULL), (2, 'nut', NULL, 'loose')",
];

// ---------------------------------------------------------------------------
// SET DEFAULT / DROP DEFAULT
// ---------------------------------------------------------------------------

#[test]
fn set_default_is_used_by_later_inserts_and_leaves_existing_rows() {
    for family in FAMILIES {
        let db = setup(ITEMS);
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 7");
        ok(&db, family, "ALTER TABLE items ALTER note SET DEFAULT 'n/a'");
        // Existing rows keep what they hold, NULL included.
        assert_eq!(
            text_rows(&db, "SELECT id, qty, note FROM items ORDER BY id"),
            vec![vec!["1", "5", "NULL"], vec!["2", "NULL", "loose"]]
        );
        ok(&db, Family::Text, "INSERT INTO items (id, name) VALUES (3, 'washer')");
        db.execute_params(
            "INSERT INTO items (id, name) VALUES ($1, $2)",
            &[Value::Int4(4), Value::String("pin".into())],
        )
        .unwrap();
        ok(&db, family, "INSERT INTO items VALUES (5, 'clip', DEFAULT, DEFAULT)");
        assert_eq!(
            text_rows(&db, "SELECT id, qty, note FROM items WHERE id >= 3 ORDER BY id"),
            vec![vec!["3", "7", "n/a"], vec!["4", "7", "n/a"], vec!["5", "7", "n/a"]]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT column_default FROM information_schema.columns WHERE table_name = 'items' AND column_name = 'qty'"
            ),
            vec![vec!["7"]]
        );

        // Replaced, then dropped.
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 1 + 1");
        ok(&db, family, "INSERT INTO items (id) VALUES (6)");
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty DROP DEFAULT");
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty DROP DEFAULT");
        ok(&db, family, "INSERT INTO items (id) VALUES (7)");
        assert_eq!(
            text_rows(&db, "SELECT id, qty FROM items WHERE id >= 6 ORDER BY id"),
            vec![vec!["6", "2"], vec!["7", "NULL"]]
        );
    }
}

#[test]
fn a_default_the_column_cannot_hold_or_a_missing_column_is_refused() {
    for family in FAMILIES {
        let db = setup(ITEMS);
        refused(&db, family, "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 'many'");
        let message = refused(&db, family, "ALTER TABLE items ALTER COLUMN nope SET DEFAULT 1");
        assert!(message.contains("nope"), "{message}");
        // Nothing changed.
        ok(&db, family, "INSERT INTO items (id) VALUES (3)");
        assert_eq!(text_rows(&db, "SELECT qty FROM items WHERE id = 3"), vec![vec!["NULL"]]);
    }
}

#[test]
fn a_volatile_default_is_evaluated_for_each_insert() {
    let db = setup(&[
        "CREATE SEQUENCE ticket_no START WITH 100",
        "CREATE TABLE tickets (id INT PRIMARY KEY, no INT8)",
    ]);
    db.execute("ALTER TABLE tickets ALTER COLUMN no SET DEFAULT nextval('ticket_no')")
        .unwrap();
    db.execute("INSERT INTO tickets (id) VALUES (1)").unwrap();
    db.execute_params("INSERT INTO tickets (id) VALUES (2)", &[]).unwrap();
    assert_eq!(
        text_rows(&db, "SELECT id, no FROM tickets ORDER BY id"),
        vec![vec!["1", "100"], vec!["2", "101"]]
    );
}

// ---------------------------------------------------------------------------
// SET NOT NULL / DROP NOT NULL
// ---------------------------------------------------------------------------

#[test]
fn set_not_null_scans_the_rows_and_is_enforced_afterwards() {
    for family in FAMILIES {
        let db = setup(ITEMS);
        // Row 2 holds NULL in qty: refused, naming the column, and nothing changes.
        let message = refused(&db, family, "ALTER TABLE items ALTER COLUMN qty SET NOT NULL");
        assert!(
            message.contains("qty") && message.contains("null"),
            "{family:?}: {message}"
        );
        ok(&db, family, "INSERT INTO items (id, name) VALUES (3, 'still nullable')");

        ok(&db, family, "UPDATE items SET qty = 0 WHERE qty IS NULL");
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty SET NOT NULL");
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty SET NOT NULL");
        let message = refused(&db, family, "INSERT INTO items (id, name) VALUES (4, 'x')");
        assert!(message.to_lowercase().contains("null"), "{family:?}: {message}");
        let message = refused(&db, family, "UPDATE items SET qty = NULL WHERE id = 1");
        assert!(message.to_lowercase().contains("null"), "{family:?}: {message}");
        assert_eq!(
            text_rows(
                &db,
                "SELECT is_nullable FROM information_schema.columns WHERE table_name = 'items' AND column_name = 'qty'"
            ),
            vec![vec!["NO"]]
        );

        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty DROP NOT NULL");
        ok(&db, family, "INSERT INTO items (id, name) VALUES (4, 'x')");
    }
}

// ---------------------------------------------------------------------------
// TYPE
// ---------------------------------------------------------------------------

#[test]
fn a_type_change_rewrites_the_rows_and_the_columns_indexes() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE m (id INT PRIMARY KEY, code INT UNIQUE, amount INT, label TEXT)",
            "CREATE INDEX m_amount_idx ON m (amount)",
            "INSERT INTO m VALUES (1, 100, 10, '7'), (2, 200, 20, '8'), (3, 300, 20, '9')",
        ]);
        ok(&db, family, "ALTER TABLE m ALTER COLUMN amount TYPE BIGINT");
        ok(&db, family, "ALTER TABLE m ALTER COLUMN code SET DATA TYPE BIGINT");
        ok(&db, family, "ALTER TABLE m ALTER COLUMN id TYPE BIGINT");
        ok(
            &db,
            family,
            "ALTER TABLE m ALTER COLUMN label TYPE INT USING label::INT * 10",
        );
        assert_eq!(
            rows(&db, "SELECT id, code, amount, label FROM m ORDER BY id"),
            vec![
                vec![Value::Int8(1), Value::Int8(100), Value::Int8(10), Value::Int4(70)],
                vec![Value::Int8(2), Value::Int8(200), Value::Int8(20), Value::Int4(80)],
                vec![Value::Int8(3), Value::Int8(300), Value::Int8(20), Value::Int4(90)],
            ]
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'm' \
                 ORDER BY ordinal_position"
            ),
            vec![
                vec!["id", "Int8"],
                vec!["code", "Int8"],
                vec!["amount", "Int8"],
                vec!["label", "Int4"]
            ]
        );
        // Every index finds its rows by a value of the new type.
        assert_eq!(text_rows(&db, "SELECT code FROM m WHERE id = 2"), vec![vec!["200"]]);
        assert_eq!(text_rows(&db, "SELECT id FROM m WHERE code = 300"), vec![vec!["3"]]);
        assert_eq!(
            text_rows(&db, "SELECT id FROM m WHERE amount = 20 ORDER BY id"),
            vec![vec!["2"], vec!["3"]]
        );
        // A value only the new type holds, and the keys still refuse a duplicate.
        ok(
            &db,
            family,
            "INSERT INTO m VALUES (5000000000, 6000000000, 7000000000, 1)",
        );
        assert_eq!(
            text_rows(&db, "SELECT amount FROM m WHERE id = 5000000000"),
            vec![vec!["7000000000"]]
        );
        let message = refused(&db, family, "INSERT INTO m VALUES (2, 999, 1, 1)");
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        let message = refused(&db, family, "INSERT INTO m VALUES (9, 200, 1, 1)");
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        ok(&db, family, "UPDATE m SET amount = 21 WHERE id = 3");
        assert_eq!(text_rows(&db, "SELECT id FROM m WHERE amount = 20"), vec![vec!["2"]]);
        ok(&db, family, "DELETE FROM m WHERE code = 100");
        assert!(text_rows(&db, "SELECT id FROM m WHERE id = 1").is_empty());
    }
}

#[test]
fn a_value_that_cannot_be_cast_names_its_row_and_nothing_is_written() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE s (id INT PRIMARY KEY, v TEXT)",
            "INSERT INTO s VALUES (1, '10'), (2, 'eleven'), (3, '12')",
        ]);
        let message = refused(&db, family, "ALTER TABLE s ALTER COLUMN v TYPE INT");
        assert!(
            message.contains("eleven") && message.contains("row"),
            "{family:?}: {message}"
        );
        assert!(message.contains("\"v\""), "{family:?}: {message}");
        assert_eq!(
            rows(&db, "SELECT v FROM s ORDER BY id"),
            vec![
                vec![Value::String("10".into())],
                vec![Value::String("eleven".into())],
                vec![Value::String("12".into())]
            ]
        );
        // With a USING that handles it.
        ok(
            &db,
            family,
            "ALTER TABLE s ALTER COLUMN v TYPE INT USING CASE WHEN v = 'eleven' THEN 11 ELSE v::INT END",
        );
        assert_eq!(
            rows(&db, "SELECT v FROM s ORDER BY id"),
            vec![vec![Value::Int4(10)], vec![Value::Int4(11)], vec![Value::Int4(12)]]
        );
    }
}

#[test]
fn a_type_change_revalidates_keys_checks_and_defaults() {
    for family in FAMILIES {
        // Two texts that are one integer: the UNIQUE key refuses the change.
        let db = setup(&[
            "CREATE TABLE u (id INT PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO u VALUES (1, '7'), (2, '07')",
        ]);
        let message = refused(&db, family, "ALTER TABLE u ALTER COLUMN v TYPE INT");
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        assert_eq!(text_rows(&db, "SELECT id FROM u WHERE v = '07'"), vec![vec!["2"]]);

        // A CHECK the new values break.
        let db = setup(&[
            "CREATE TABLE c (id INT PRIMARY KEY, v NUMERIC CHECK (v > 0))",
            "INSERT INTO c VALUES (1, 0.4), (2, 3.2)",
        ]);
        let message = refused(&db, family, "ALTER TABLE c ALTER COLUMN v TYPE INT");
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        assert_eq!(text_rows(&db, "SELECT count(*) FROM c WHERE v < 1"), vec![vec!["1"]]);

        // A default that has no value of the new type.
        let db = setup(&["CREATE TABLE d (id INT PRIMARY KEY, v TEXT DEFAULT 'none')"]);
        let message = refused(&db, family, "ALTER TABLE d ALTER COLUMN v TYPE INT");
        assert!(message.contains("default"), "{family:?}: {message}");
        ok(&db, family, "ALTER TABLE d ALTER COLUMN v DROP DEFAULT");
        ok(&db, family, "ALTER TABLE d ALTER COLUMN v TYPE INT");

        // A default that does: it is kept and gives a value of the new type.
        let db = setup(&["CREATE TABLE e (id INT PRIMARY KEY, v INT DEFAULT 5)"]);
        ok(&db, family, "ALTER TABLE e ALTER COLUMN v TYPE TEXT");
        ok(&db, family, "INSERT INTO e (id) VALUES (1)");
        assert_eq!(rows(&db, "SELECT v FROM e"), vec![vec![Value::String("5".into())]]);
    }
}

#[test]
fn a_type_change_is_refused_where_something_else_reads_the_column() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE p (id INT PRIMARY KEY, label TEXT)",
            "INSERT INTO p VALUES (1, 'a')",
            "CREATE VIEW p_labels AS SELECT id, label FROM p",
            "CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES p(id))",
            "INSERT INTO ch VALUES (1, 1)",
        ]);
        let message = refused(&db, family, "ALTER TABLE p ALTER COLUMN label TYPE VARCHAR(10)");
        assert!(
            message.contains("p_labels") && message.contains("is not supported"),
            "{family:?}: {message}"
        );
        ok(&db, family, "DROP VIEW p_labels");
        ok(&db, family, "ALTER TABLE p ALTER COLUMN label TYPE VARCHAR(10)");

        // A foreign key column: between integer types the values keep their meaning.
        let message = refused(&db, family, "ALTER TABLE ch ALTER COLUMN pid TYPE TEXT");
        assert!(
            message.contains("foreign key") && message.contains("is not supported"),
            "{family:?}: {message}"
        );
        ok(&db, family, "ALTER TABLE p ALTER COLUMN id TYPE BIGINT");
        ok(&db, family, "INSERT INTO ch VALUES (2, 1)");
        let message = refused(&db, family, "INSERT INTO ch VALUES (3, 99)");
        assert!(message.to_lowercase().contains("foreign key"), "{family:?}: {message}");
        ok(&db, family, "ALTER TABLE ch ALTER COLUMN pid TYPE BIGINT");
        ok(&db, family, "INSERT INTO ch VALUES (4, 1)");
        let message = refused(&db, family, "DELETE FROM p WHERE id = 1");
        assert!(message.to_lowercase().contains("foreign key"), "{family:?}: {message}");
    }
}

/// `AS OF TIMESTAMP` resolves to the second: a pause either side of a mark keeps
/// every write on its own side of it.
fn mark() -> String {
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    now
}

/// A version from before the change holds a value of the old type under a
/// column of the new one: the read is refused, as it is across a rename.
#[test]
fn a_read_as_of_a_time_before_a_type_change_is_refused() {
    let db = setup(&[
        "CREATE TABLE h (id INT PRIMARY KEY, v TEXT)",
        "INSERT INTO h VALUES (1, '5')",
    ]);
    let before = mark();
    assert_eq!(
        rows(&db, &format!("SELECT v FROM h AS OF TIMESTAMP '{before}'")),
        vec![vec![Value::String("5".into())]]
    );
    db.execute("ALTER TABLE h ALTER COLUMN v TYPE INT").unwrap();
    let after = mark();
    assert_eq!(rows(&db, "SELECT v FROM h"), vec![vec![Value::Int4(5)]]);
    assert_eq!(
        rows(&db, &format!("SELECT v FROM h AS OF TIMESTAMP '{after}'")),
        vec![vec![Value::Int4(5)]]
    );
    let error = db
        .query(&format!("SELECT v FROM h AS OF TIMESTAMP '{before}'"), &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("type changed") && error.contains("\"h\""), "{error}");
}

// ---------------------------------------------------------------------------
// IDENTITY, STATISTICS
// ---------------------------------------------------------------------------

#[test]
fn add_and_drop_identity() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE k (id INT PRIMARY KEY, v TEXT)",
            "INSERT INTO k VALUES (1, 'a'), (2, 'b')",
        ]);
        let identity =
            "SELECT is_identity FROM information_schema.columns WHERE table_name = 'k' AND column_name = 'id'";
        assert_ne!(text_rows(&db, identity), vec![vec!["YES"]]);
        ok(
            &db,
            family,
            "ALTER TABLE k ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY",
        );
        assert_eq!(text_rows(&db, identity), vec![vec!["YES"]]);
        // A key is generated for a row that names none, above the keys in use.
        ok(&db, family, "INSERT INTO k (v) VALUES ('c')");
        let generated = text_rows(&db, "SELECT id FROM k WHERE v = 'c'");
        assert_eq!(generated.len(), 1);
        assert!(generated[0][0].parse::<i64>().unwrap() > 2, "{generated:?}");
        refused(
            &db,
            family,
            "ALTER TABLE k ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY",
        );
        // Only an integer key can be one.
        let message = refused(
            &db,
            family,
            "ALTER TABLE k ALTER COLUMN v ADD GENERATED ALWAYS AS IDENTITY",
        );
        assert!(message.contains("\"v\""), "{family:?}: {message}");

        ok(&db, family, "ALTER TABLE k ALTER COLUMN id DROP IDENTITY");
        assert_ne!(text_rows(&db, identity), vec![vec!["YES"]]);
        let message = refused(&db, family, "INSERT INTO k (v) VALUES ('d')");
        assert!(message.to_lowercase().contains("null"), "{family:?}: {message}");
        ok(&db, family, "INSERT INTO k VALUES (50, 'd')");
        ok(&db, family, "ALTER TABLE k ALTER COLUMN id DROP IDENTITY IF EXISTS");
        let message = refused(&db, family, "ALTER TABLE k ALTER COLUMN id DROP IDENTITY");
        assert!(message.contains("not an identity column"), "{family:?}: {message}");
        assert_eq!(text_rows(&db, "SELECT count(*) FROM k"), vec![vec!["4"]]);
    }
}

#[test]
fn set_statistics_is_stored_and_shown_in_the_catalog() {
    let target = "SELECT attstattarget FROM pg_attribute WHERE attname = 'qty'";
    for family in FAMILIES {
        let db = setup(ITEMS);
        assert_eq!(text_rows(&db, target), vec![vec!["-1"]]);
        ok(&db, family, "ALTER TABLE items ALTER COLUMN qty SET STATISTICS 500");
        assert_eq!(text_rows(&db, target), vec![vec!["500"]]);
        // It follows a rename, and goes with the default.
        ok(&db, family, "ALTER TABLE items RENAME COLUMN qty TO quantity");
        assert_eq!(
            text_rows(&db, "SELECT attstattarget FROM pg_attribute WHERE attname = 'quantity'"),
            vec![vec!["500"]]
        );
        ok(&db, family, "ALTER TABLE items ALTER COLUMN quantity SET STATISTICS -1");
        assert_eq!(
            text_rows(&db, "SELECT attstattarget FROM pg_attribute WHERE attname = 'quantity'"),
            vec![vec!["-1"]]
        );
        refused(
            &db,
            family,
            "ALTER TABLE items ALTER COLUMN quantity SET STATISTICS 20000",
        );
        // Several clauses in one statement.
        ok(
            &db,
            family,
            "ALTER TABLE items ALTER COLUMN quantity SET STATISTICS 10, ALTER COLUMN name SET STATISTICS 20",
        );
        assert_eq!(
            text_rows(
                &db,
                "SELECT attname, attstattarget FROM pg_attribute WHERE attname IN ('quantity', 'name') ORDER BY attname"
            ),
            vec![vec!["name", "20"], vec!["quantity", "10"]]
        );
    }
}

// ---------------------------------------------------------------------------
// One statement, a restart, the wire
// ---------------------------------------------------------------------------

#[test]
fn several_clauses_are_one_statement_all_or_nothing() {
    for family in FAMILIES {
        let db = setup(ITEMS);
        // The second clause fails (row 2 holds NULL in qty): the first is not kept.
        refused(
            &db,
            family,
            "ALTER TABLE items ALTER COLUMN note SET DEFAULT 'x', ALTER COLUMN qty SET NOT NULL",
        );
        ok(&db, family, "INSERT INTO items (id) VALUES (3)");
        assert_eq!(
            text_rows(&db, "SELECT note FROM items WHERE id = 3"),
            vec![vec!["NULL"]]
        );

        ok(&db, family, "UPDATE items SET qty = 1 WHERE qty IS NULL");
        ok(
            &db,
            family,
            "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 9, ALTER COLUMN qty SET NOT NULL, \
             ALTER COLUMN qty TYPE BIGINT, ADD COLUMN extra INT DEFAULT 4",
        );
        ok(&db, family, "INSERT INTO items (id) VALUES (4)");
        assert_eq!(
            rows(&db, "SELECT qty, extra FROM items WHERE id = 4"),
            vec![vec![Value::Int8(9), Value::Int4(4)]]
        );
    }
}

#[test]
fn every_form_survives_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = open(dir.path());
        for statement in ITEMS {
            db.execute(statement).unwrap();
        }
        db.execute("UPDATE items SET qty = 0 WHERE qty IS NULL").unwrap();
        db.execute("ALTER TABLE items ALTER COLUMN qty SET DEFAULT 42").unwrap();
        db.execute("ALTER TABLE items ALTER COLUMN qty SET NOT NULL").unwrap();
        db.execute("ALTER TABLE items ALTER COLUMN qty TYPE BIGINT").unwrap();
        db.execute("ALTER TABLE items ALTER COLUMN name SET STATISTICS 300")
            .unwrap();
        db.execute("ALTER TABLE items ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY")
            .unwrap();
        db.execute("CREATE INDEX items_qty_idx ON items (qty)").unwrap();
        db.close().unwrap();
    }
    let db = open(dir.path());
    db.execute("INSERT INTO items (name) VALUES ('after restart')").unwrap();
    assert_eq!(
        rows(&db, "SELECT qty FROM items WHERE name = 'after restart'"),
        vec![vec![Value::Int8(42)]]
    );
    assert_eq!(
        rows(&db, "SELECT qty FROM items WHERE id = 1"),
        vec![vec![Value::Int8(5)]]
    );
    assert_eq!(text_rows(&db, "SELECT id FROM items WHERE qty = 5"), vec![vec!["1"]]);
    refused(&db, Family::Text, "UPDATE items SET qty = NULL WHERE id = 1");
    assert_eq!(
        text_rows(&db, "SELECT attstattarget FROM pg_attribute WHERE attname = 'name'"),
        vec![vec!["300"]]
    );
    assert_eq!(
        text_rows(
            &db,
            "SELECT is_identity FROM information_schema.columns WHERE table_name = 'items' AND column_name = 'id'"
        ),
        vec![vec!["YES"]]
    );
}

#[test]
fn a_form_with_no_support_is_refused_in_sql_words() {
    for family in FAMILIES {
        let db = setup(ITEMS);
        for (statement, words) in [
            (
                "ALTER TABLE items ALTER COLUMN name SET COMPRESSION lz4",
                "SET COMPRESSION",
            ),
            (
                "ALTER TABLE items ALTER COLUMN id SET GENERATED ALWAYS",
                "SET GENERATED",
            ),
            ("ALTER TABLE items ALTER COLUMN name SET (n_distinct = 5)", "SET ("),
            ("ALTER TABLE items ENABLE ALWAYS RULE some_rule", "ENABLE ALWAYS RULE"),
            ("ALTER TABLE items DISABLE RULE some_rule", "DISABLE RULE"),
        ] {
            let message = refused(&db, family, statement);
            assert!(message.contains("is not supported"), "{statement}: {message}");
            assert!(message.contains(words), "{statement}: {message}");
            assert!(
                !message.contains("Unsupported ALTER TABLE operation"),
                "{statement}: {message}"
            );
            assert!(!message.contains('{'), "a debug rendering leaked: {message}");
        }
    }
}

#[tokio::test]
async fn over_the_wire() {
    let server = serve(setup(ITEMS)).await;
    let (client, task) = connect(&server).await;
    wire_ok(&client, "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 3").await;
    // A prepared INSERT picks the default up.
    let insert = client
        .prepare("INSERT INTO items (id, name) VALUES ($1, $2)")
        .await
        .unwrap();
    client.execute(&insert, &[&10i32, &"a"]).await.unwrap();
    wire_ok(&client, "ALTER TABLE items ALTER COLUMN qty SET DEFAULT 4").await;
    client.execute(&insert, &[&11i32, &"b"]).await.unwrap();
    assert_eq!(
        wire_rows(&client, "SELECT id, qty FROM items WHERE id >= 10 ORDER BY id").await,
        vec![vec!["10", "3"], vec!["11", "4"]]
    );

    let (code, message) = wire_refused(&client, "ALTER TABLE items ALTER COLUMN qty SET NOT NULL").await;
    assert_eq!(code, "23502", "{message}");
    assert!(message.contains("qty"), "{message}");

    wire_ok(&client, "UPDATE items SET note = '1' WHERE note IS NULL").await;
    let (code, message) = wire_refused(&client, "ALTER TABLE items ALTER COLUMN note TYPE INT").await;
    assert_ne!(code, "XX000", "{message}");
    assert!(message.contains("loose"), "{message}");
    wire_ok(&client, "ALTER TABLE items ALTER COLUMN qty TYPE BIGINT").await;
    client
        .execute(
            "INSERT INTO items (id, qty) VALUES ($1, $2)",
            &[&12i32, &9_000_000_000i64],
        )
        .await
        .unwrap();
    assert_eq!(
        wire_rows(&client, "SELECT qty FROM items WHERE id = 12").await,
        vec![vec!["9000000000"]]
    );

    for statement in [
        "ALTER TABLE items ALTER COLUMN name SET COMPRESSION lz4",
        "ALTER TABLE items DISABLE RULE some_rule",
        "ALTER TABLE items ALTER COLUMN name ADD GENERATED ALWAYS AS IDENTITY",
    ] {
        let (code, message) = wire_refused(&client, statement).await;
        assert_ne!(code, "XX000", "{statement}: {message}");
    }
    let (code, message) = wire_refused(&client, "ALTER TABLE items ALTER COLUMN name SET COMPRESSION lz4").await;
    assert_eq!(code, "0A000", "{message}");
    let (code, message) = wire_refused(&client, "ALTER TABLE items DISABLE RULE some_rule").await;
    assert_eq!(code, "0A000", "{message}");
    wire_ok(&client, "ALTER TABLE items ALTER COLUMN name SET STATISTICS 100").await;
    wire_ok(&client, "ALTER TABLE items ALTER COLUMN id DROP IDENTITY IF EXISTS").await;
    task.abort();
}
