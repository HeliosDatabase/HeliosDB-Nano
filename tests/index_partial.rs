//! Partial indexes: `CREATE [UNIQUE] INDEX … WHERE predicate`.
//!
//! Before: the predicate was never read. A partial UNIQUE index (the
//! soft-delete pattern, `WHERE deleted_at IS NULL`) was enforced on every row,
//! so a deleted row's e-mail could never be used again, and `pg_indexes` and a
//! dump showed an index over the whole table. Now the predicate is stored; the
//! index holds only the rows that satisfy it; uniqueness is enforced among
//! those rows only; a query reads through the index only when its own predicate
//! implies the index's, and `EXPLAIN` names it.
//!
//! Both statement families; the main cases over the PostgreSQL wire; a reopen;
//! a dump/restore round trip in both formats; a physical snapshot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod index_forms_common;

use std::sync::Arc;

use heliosdb_nano::{EmbeddedDatabase, Value};
use index_forms_common::*;

fn users(db: &EmbeddedDatabase, params: bool) {
    must(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        params,
    );
    must(
        db,
        "CREATE UNIQUE INDEX users_email_live ON users (email) WHERE deleted_at IS NULL",
        params,
    );
}

fn is_duplicate(err: &heliosdb_nano::Error, index: &str) -> bool {
    let text = err.to_string();
    text.to_lowercase().contains("unique") && text.contains(index)
}

/// The reproduction: a soft-deleted row no longer blocks its e-mail.
#[test]
fn uniqueness_holds_among_the_rows_of_the_predicate_only() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        users(&db, params);
        must(&db, "INSERT INTO users VALUES (1, 'a@x.io', NULL)", params);
        // Outside the predicate: any number of them.
        must(&db, "INSERT INTO users VALUES (2, 'a@x.io', '2026-01-01')", params);
        must(&db, "INSERT INTO users VALUES (3, 'a@x.io', '2026-01-02')", params);
        // Inside it: one.
        let err = run(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");
        // A NULL key part is never a duplicate.
        must(&db, "INSERT INTO users VALUES (5, NULL, NULL), (6, NULL, NULL)", params);

        // Soft-delete the live row: its e-mail is free again.
        must(&db, "UPDATE users SET deleted_at = '2026-02-01' WHERE id = 1", params);
        must(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params);
        // Reviving a deleted row would make two live ones.
        let err = run(&db, "UPDATE users SET deleted_at = NULL WHERE id = 2", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");
        // Changing a live row's e-mail onto another live row's.
        must(&db, "INSERT INTO users VALUES (7, 'b@x.io', NULL)", params);
        let err = run(&db, "UPDATE users SET email = 'a@x.io' WHERE id = 7", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");
        // A deleted row may take any e-mail.
        must(&db, "UPDATE users SET email = 'b@x.io' WHERE id = 3", params);
        // One statement moving two rows onto one live key is refused whole.
        must(
            &db,
            "INSERT INTO users VALUES (8, 'c@x.io', NULL), (9, 'd@x.io', NULL)",
            params,
        );
        let err = run(&db, "UPDATE users SET email = 'z@x.io' WHERE id IN (8, 9)", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");
        assert_eq!(
            id_set(&db, "SELECT id FROM users WHERE email = 'z@x.io'", params),
            Vec::<i64>::new()
        );
        // Deleting the live row frees the key.
        must(&db, "DELETE FROM users WHERE id = 4", params);
        must(&db, "UPDATE users SET deleted_at = NULL WHERE id = 2", params);

        assert_eq!(
            id_set(
                &db,
                "SELECT id FROM users WHERE email = 'a@x.io' AND deleted_at IS NULL",
                params
            ),
            vec![2]
        );
        assert_eq!(
            id_set(&db, "SELECT id FROM users WHERE email = 'a@x.io'", params),
            vec![1, 2]
        );
    }
}

#[test]
fn a_rolled_back_transaction_gives_its_keys_back() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        users(&db, params);
        must(&db, "INSERT INTO users VALUES (1, 'a@x.io', NULL)", params);

        must(&db, "BEGIN", params);
        must(&db, "INSERT INTO users VALUES (2, 'b@x.io', NULL)", params);
        must(&db, "ROLLBACK", params);
        must(&db, "INSERT INTO users VALUES (3, 'b@x.io', NULL)", params);

        // A rolled-back soft delete: the key is the row's again.
        must(&db, "BEGIN", params);
        must(&db, "UPDATE users SET deleted_at = 'now' WHERE id = 1", params);
        must(&db, "ROLLBACK", params);
        let err = run(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");

        // A rolled-back DELETE too.
        must(&db, "BEGIN", params);
        must(&db, "DELETE FROM users WHERE id = 1", params);
        must(&db, "ROLLBACK", params);
        let err = run(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");

        // Committed: the soft delete frees it, inside one transaction.
        must(&db, "BEGIN", params);
        must(&db, "UPDATE users SET deleted_at = 'now' WHERE id = 1", params);
        must(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params);
        must(&db, "COMMIT", params);
        assert_eq!(
            id_set(
                &db,
                "SELECT id FROM users WHERE email = 'a@x.io' AND deleted_at IS NULL",
                params
            ),
            vec![4]
        );
    }
}

#[test]
fn rows_that_already_break_it_refuse_the_index() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        must(
            &db,
            "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
            params,
        );
        must(
            &db,
            "INSERT INTO users VALUES (1, 'a@x.io', NULL), (2, 'a@x.io', 'gone'), (3, 'a@x.io', NULL)",
            params,
        );
        let err = run(
            &db,
            "CREATE UNIQUE INDEX users_email_live ON users (email) WHERE deleted_at IS NULL",
            params,
        )
        .unwrap_err();
        assert!(err.to_string().contains("users_email_live"), "{err}");
        // Nothing is left behind, and nothing is enforced.
        let left = rows(
            &db,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'users'",
            params,
        );
        assert!(left.iter().all(|r| text(&r[0]) != "users_email_live"), "{left:?}");
        must(&db, "INSERT INTO users VALUES (4, 'a@x.io', NULL)", params);
        // With the duplicates soft-deleted it builds.
        must(&db, "UPDATE users SET deleted_at = 'gone' WHERE id IN (3, 4)", params);
        must(
            &db,
            "CREATE UNIQUE INDEX users_email_live ON users (email) WHERE deleted_at IS NULL",
            params,
        );
        let err = run(&db, "INSERT INTO users VALUES (5, 'a@x.io', NULL)", params).unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{err}");
    }
}

/// `orders` (indexed) and `twin` (no index): 300 rows, a tenth of them open.
fn orders(db: &EmbeddedDatabase, params: bool) {
    for table in ["orders", "twin"] {
        must(
            db,
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, customer TEXT, status TEXT, amount INT)"),
            params,
        );
        let mut values = Vec::new();
        for id in 0..300 {
            let status = if id % 10 == 0 { "open" } else { "closed" };
            values.push(format!("({id}, 'c{}', '{status}', {})", id % 7, id * 10));
        }
        must(db, &format!("INSERT INTO {table} VALUES {}", values.join(", ")), params);
    }
    must(
        db,
        "CREATE INDEX orders_open ON orders (customer) WHERE status = 'open'",
        params,
    );
    must(
        db,
        "CREATE INDEX orders_big ON orders (amount) WHERE amount > 2000",
        params,
    );
}

fn same_as_twin(db: &EmbeddedDatabase, tail: &str, params: bool) -> Vec<i64> {
    let indexed = id_set(db, &format!("SELECT id FROM orders WHERE {tail}"), params);
    let twin = id_set(db, &format!("SELECT id FROM twin WHERE {tail}"), params);
    assert_eq!(indexed, twin, "`{tail}` (params={params})");
    indexed
}

#[test]
fn a_query_reads_through_it_only_when_its_predicate_implies_the_index_predicate() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        orders(&db, params);

        // The same predicate, with a condition on the key.
        let p = plan(
            &db,
            "SELECT id FROM orders WHERE customer = 'c3' AND status = 'open'",
            params,
        );
        assert!(p.contains("Index Scan using orders_open on orders"), "{p}");
        assert!(p.contains("index predicate: status = 'open'"), "{p}");
        assert_eq!(
            same_as_twin(&db, "customer = 'c3' AND status = 'open'", params).len(),
            5
        );
        // Written the other way round.
        let p = plan(
            &db,
            "SELECT id FROM orders WHERE 'open' = status AND customer = 'c3'",
            params,
        );
        assert!(p.contains("Index Scan using orders_open"), "{p}");
        // The predicate alone: the index is the list of open orders.
        let p = plan(&db, "SELECT id FROM orders WHERE status = 'open'", params);
        assert!(p.contains("Index Scan using orders_open"), "{p}");
        assert_eq!(same_as_twin(&db, "status = 'open'", params).len(), 30);

        // Not implied: the index would leave rows out, so it is not used.
        for tail in [
            "customer = 'c3'",
            "customer = 'c3' AND status = 'closed'",
            "customer = 'c3' AND status <> 'open'",
        ] {
            let p = plan(&db, &format!("SELECT id FROM orders WHERE {tail}"), params);
            assert!(!p.contains("orders_open"), "`{tail}`: {p}");
            same_as_twin(&db, tail, params);
        }
        // Under an OR the predicate does not hold for every row asked for.
        let tail = "customer = 'c3' AND (status = 'open' OR amount > 100)";
        assert!(!plan(&db, &format!("SELECT id FROM orders WHERE {tail}"), params).contains("orders_open"));
        same_as_twin(&db, tail, params);

        // A range predicate is implied by a tighter range, not by a wider one.
        let p = plan(&db, "SELECT id FROM orders WHERE amount > 2500", params);
        assert!(p.contains("Index Scan using orders_big"), "{p}");
        assert_eq!(same_as_twin(&db, "amount > 2500", params).len(), 49);
        assert!(plan(
            &db,
            "SELECT id FROM orders WHERE amount >= 2001 AND amount < 2100",
            params
        )
        .contains("orders_big"));
        same_as_twin(&db, "amount >= 2001 AND amount < 2100", params);
        assert!(plan(&db, "SELECT id FROM orders WHERE amount = 2990", params).contains("orders_big"));
        assert_eq!(same_as_twin(&db, "amount = 2990", params), vec![299]);
        for tail in ["amount > 1500", "amount >= 2000", "amount = 2000", "amount < 2500"] {
            let p = plan(&db, &format!("SELECT id FROM orders WHERE {tail}"), params);
            assert!(!p.contains("orders_big"), "`{tail}`: {p}");
            same_as_twin(&db, tail, params);
        }
    }
}

#[test]
fn a_bound_parameter_decides_per_execution() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    orders(&db, true);
    let sql = "SELECT id FROM orders WHERE customer = $1 AND status = $2";
    let twin = "SELECT id FROM twin WHERE customer = $1 AND status = $2";
    let open = [Value::String("c3".into()), Value::String("open".into())];
    let closed = [Value::String("c3".into()), Value::String("closed".into())];
    assert!(plan_bound(&db, sql, &open).contains("Index Scan using orders_open"));
    assert!(!plan_bound(&db, sql, &closed).contains("orders_open"));
    // One statement, both values, in either order: each answer is its own.
    for bound in [&open, &closed, &open] {
        assert_eq!(id_set_bound(&db, sql, bound), id_set_bound(&db, twin, bound));
    }
    assert_eq!(id_set_bound(&db, sql, &open).len(), 5);
    assert_eq!(id_set_bound(&db, sql, &closed).len(), 38);
}

#[test]
fn rows_enter_and_leave_it_as_they_change() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        orders(&db, params);
        for table in ["orders", "twin"] {
            // Into the predicate, out of it, and a key change inside it.
            must(&db, &format!("UPDATE {table} SET status = 'open' WHERE id = 3"), params);
            must(
                &db,
                &format!("UPDATE {table} SET status = 'closed' WHERE id = 10"),
                params,
            );
            must(
                &db,
                &format!("UPDATE {table} SET customer = 'c3' WHERE id = 20"),
                params,
            );
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (1000, 'c3', 'open', 5), (1001, 'c3', 'closed', 5)"),
                params,
            );
            must(&db, &format!("DELETE FROM {table} WHERE id = 80"), params);
            must(&db, &format!("UPDATE {table} SET amount = 9000 WHERE id = 1"), params);
            must(&db, &format!("UPDATE {table} SET amount = 1 WHERE id = 299"), params);
        }
        let got = same_as_twin(&db, "customer = 'c3' AND status = 'open'", params);
        assert_eq!(got, vec![3, 20, 150, 220, 290, 1000]);
        assert!(plan(
            &db,
            "SELECT id FROM orders WHERE customer = 'c3' AND status = 'open'",
            params
        )
        .contains("Index Scan using orders_open"));
        assert_eq!(same_as_twin(&db, "status = 'open'", params).len(), 30);
        let big = same_as_twin(&db, "amount > 2900", params);
        assert!(big.contains(&1) && !big.contains(&299), "{big:?}");
    }
}

#[test]
fn the_catalog_shows_the_predicate_and_alter_table_respects_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        users(&db, params);
        let def = indexdef(&db, "users_email_live", params);
        assert!(
            def.starts_with("CREATE UNIQUE INDEX users_email_live ON public.users USING btree (email)"),
            "{def}"
        );
        assert!(def.contains("WHERE (deleted_at IS NULL)"), "{def}");

        // The predicate is SQL text over the column names: a rename is refused.
        let err = run(&db, "ALTER TABLE users RENAME COLUMN deleted_at TO gone_at", params).unwrap_err();
        assert!(err.to_string().contains("users_email_live"), "{err}");
        // A column the predicate reads goes, and the index with it.
        must(&db, "ALTER TABLE users DROP COLUMN deleted_at", params);
        let left = rows(
            &db,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'users'",
            params,
        );
        assert!(left.iter().all(|r| text(&r[0]) != "users_email_live"), "{left:?}");
        must(&db, "INSERT INTO users VALUES (1, 'a@x.io'), (2, 'a@x.io')", params);
    }
}

#[test]
fn what_is_not_supported_is_refused_by_name() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        must(
            &db,
            "CREATE TABLE d (id INT PRIMARY KEY, body TEXT, seen_at TEXT, n INT)",
            params,
        );
        // A predicate whose value changes with time is not a predicate an index can keep.
        let err = run(&db, "CREATE INDEX d_recent ON d (n) WHERE seen_at > now()", params).unwrap_err();
        assert!(err.to_string().contains("must be marked IMMUTABLE"), "{err}");
        let err = run(
            &db,
            "CREATE INDEX d_sub ON d (n) WHERE n > (SELECT max(n) FROM d)",
            params,
        )
        .unwrap_err();
        assert!(err.to_string().contains("subquery"), "{err}");
        // A column the table does not have.
        let err = run(&db, "CREATE INDEX d_nope ON d (n) WHERE nope IS NULL", params).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        // Only the ordered tree keeps a partial index.
        let err = run(&db, "CREATE INDEX d_gin ON d USING gin (body) WHERE n > 0", params).unwrap_err();
        assert!(err.to_string().contains("is not supported"), "{err}");
        let left = rows(&db, "SELECT indexname FROM pg_indexes WHERE tablename = 'd'", params);
        assert!(left.iter().all(|r| !text(&r[0]).starts_with("d_")), "{left:?}");
    }
}

#[test]
fn on_conflict_do_nothing_skips_a_row_the_partial_index_refuses() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        users(&db, params);
        must(&db, "INSERT INTO users VALUES (1, 'a@x.io', NULL)", params);
        must(
            &db,
            "INSERT INTO users VALUES (2, 'a@x.io', NULL) ON CONFLICT DO NOTHING",
            params,
        );
        must(
            &db,
            "INSERT INTO users VALUES (3, 'a@x.io', 'gone') ON CONFLICT DO NOTHING",
            params,
        );
        assert_eq!(id_set(&db, "SELECT id FROM users", params), vec![1, 3]);
    }
}

#[test]
fn it_survives_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        users(&db, false);
        orders(&db, false);
        db.execute("INSERT INTO users VALUES (1, 'a@x.io', NULL), (2, 'a@x.io', 'gone')")
            .unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    for params in FAMILIES {
        let def = indexdef(&db, "users_email_live", params);
        assert!(def.contains("WHERE (deleted_at IS NULL)"), "{def}");
        let p = plan(
            &db,
            "SELECT id FROM orders WHERE customer = 'c3' AND status = 'open'",
            params,
        );
        assert!(p.contains("Index Scan using orders_open"), "{p}");
        assert_eq!(
            same_as_twin(&db, "customer = 'c3' AND status = 'open'", params).len(),
            5
        );
        assert!(!plan(&db, "SELECT id FROM orders WHERE customer = 'c3'", params).contains("orders_open"));
        same_as_twin(&db, "customer = 'c3'", params);
    }
    // Still partial: a deleted duplicate is accepted, a live one is not.
    db.execute("INSERT INTO users VALUES (3, 'a@x.io', 'gone')").unwrap();
    let err = db.execute("INSERT INTO users VALUES (4, 'a@x.io', NULL)").unwrap_err();
    assert!(is_duplicate(&err, "users_email_live"), "{err}");
}

#[test]
fn a_dump_carries_the_predicate() {
    for format in FORMATS {
        let src = EmbeddedDatabase::new_in_memory().unwrap();
        users(&src, false);
        orders(&src, false);
        src.execute("INSERT INTO users VALUES (1, 'a@x.io', NULL), (2, 'a@x.io', 'gone')")
            .unwrap();
        let (db, _dir) = round_trip(&src, format);
        let def = indexdef(&db, "users_email_live", false);
        assert!(
            def.contains("UNIQUE") && def.contains("WHERE (deleted_at IS NULL)"),
            "{format:?}: {def}"
        );
        db.execute("INSERT INTO users VALUES (3, 'a@x.io', 'gone')")
            .unwrap_or_else(|e| panic!("{format:?}: a deleted duplicate must be accepted after a restore: {e}"));
        let err = db.execute("INSERT INTO users VALUES (4, 'a@x.io', NULL)").unwrap_err();
        assert!(is_duplicate(&err, "users_email_live"), "{format:?}: {err}");
        let p = plan(
            &db,
            "SELECT id FROM orders WHERE customer = 'c3' AND status = 'open'",
            false,
        );
        assert!(p.contains("Index Scan using orders_open"), "{format:?}: {p}");
        assert_eq!(same_as_twin(&db, "customer = 'c3' AND status = 'open'", false).len(), 5);
    }
}

/// The standby path: a sealed image of a database with a partial index opens,
/// lists it, and answers through it.
#[test]
fn a_physical_snapshot_opens_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = heliosdb_nano::Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.memory_only = false;
    {
        let db = EmbeddedDatabase::with_config(config.clone()).unwrap();
        users(&db, false);
        orders(&db, false);
        db.execute("INSERT INTO users VALUES (1, 'a@x.io', NULL), (2, 'a@x.io', 'gone')")
            .unwrap();
    }
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config)
        .unwrap_or_else(|e| panic!("hydrating a snapshot with a partial index: {e}"));
    let db = hydrated.database;
    for params in FAMILIES {
        let def = indexdef(&db, "users_email_live", params);
        assert!(def.contains("WHERE (deleted_at IS NULL)"), "{def}");
        assert_eq!(
            same_as_twin(&db, "customer = 'c3' AND status = 'open'", params).len(),
            5
        );
        assert_eq!(
            id_set(
                &db,
                "SELECT id FROM users WHERE email = 'a@x.io' AND deleted_at IS NULL",
                params
            ),
            vec![1]
        );
        assert_eq!(
            id_set(&db, "SELECT id FROM users WHERE email = 'a@x.io'", params),
            vec![1, 2]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_the_postgresql_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (conn, server) = serve(Arc::clone(&db)).await;
    let client = connect(&conn).await;
    for statement in [
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX users_email_live ON users (email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'a@x.io', NULL)",
        "INSERT INTO users VALUES (2, 'a@x.io', 'gone')",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    assert_eq!(
        wire_sqlstate(&client, "INSERT INTO users VALUES (3, 'a@x.io', NULL)").await,
        "23505"
    );
    // The extended protocol.
    let err = client
        .execute("INSERT INTO users VALUES ($1, $2, NULL)", &[&4i32, &"a@x.io"])
        .await
        .unwrap_err();
    assert_eq!(
        err.code().map(|c| c.code().to_string()).as_deref(),
        Some("23505"),
        "{err}"
    );
    client
        .execute("INSERT INTO users VALUES ($1, $2, $3)", &[&5i32, &"a@x.io", &"gone"])
        .await
        .unwrap();
    assert_eq!(
        wire_sqlstate(&client, "UPDATE users SET deleted_at = NULL WHERE id = 2").await,
        "23505"
    );
    let p: Vec<String> = wire_rows(
        &client,
        "EXPLAIN SELECT id FROM users WHERE email = 'a@x.io' AND deleted_at IS NULL",
    )
    .await
    .into_iter()
    .flatten()
    .collect();
    assert!(p.join("\n").contains("Index Scan using users_email_live"), "{p:?}");
    assert_eq!(
        wire_rows(
            &client,
            "SELECT id FROM users WHERE email = 'a@x.io' AND deleted_at IS NULL"
        )
        .await,
        vec![vec!["1".to_string()]]
    );
    let def = wire_rows(
        &client,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'users_email_live'",
    )
    .await;
    assert!(def[0][0].contains("WHERE (deleted_at IS NULL)"), "{def:?}");
    // A predicate that is not immutable: 42P17.
    assert_eq!(
        wire_sqlstate(
            &client,
            "CREATE INDEX users_recent ON users (email) WHERE deleted_at > now()"
        )
        .await,
        "42P17"
    );
    server.abort();
}
