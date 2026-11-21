//! Expression indexes: `CREATE [UNIQUE] INDEX … (lower(email))`.
//!
//! Before: an index on an expression was recorded and nothing else. No write
//! kept it, no query read through it, and `CREATE UNIQUE INDEX` on an
//! expression was refused. Now the expression is evaluated on every write, the
//! index is kept, a query whose predicate or `ORDER BY … LIMIT` holds the same
//! expression reads through it (`EXPLAIN` names it), a UNIQUE one is enforced,
//! and an expression that is not immutable is refused (42P17).
//!
//! Both statement families; the main cases over the PostgreSQL wire; a reopen
//! (which is also how a definition recorded by the release before comes to
//! life); a dump/restore round trip in both formats.

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

/// `u` (indexed) and `twin` (no index): 200 rows, e-mails in mixed case.
fn build(db: &EmbeddedDatabase, params: bool) {
    for table in ["u", "twin"] {
        must(
            db,
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, email TEXT, a INT, b INT, payload JSONB)"),
            params,
        );
        let mut values = Vec::new();
        for id in 0..200 {
            let email = if id % 2 == 0 {
                format!("User{id:03}@X.io")
            } else {
                format!("user{id:03}@x.IO")
            };
            values.push(format!(
                "({id}, '{email}', {}, {}, '{{\"user_id\":\"u{}\",\"n\":{id}}}')",
                id % 10,
                id / 10,
                id % 25
            ));
        }
        must(db, &format!("INSERT INTO {table} VALUES {}", values.join(", ")), params);
    }
    must(db, "CREATE INDEX u_lower ON u (lower(email))", params);
    must(db, "CREATE INDEX u_uid ON u ((payload->>'user_id'))", params);
    must(db, "CREATE INDEX u_sum ON u ((a + b))", params);
}

fn same_as_twin(db: &EmbeddedDatabase, tail: &str, params: bool) -> Vec<i64> {
    let indexed = id_set(db, &format!("SELECT id FROM u WHERE {tail}"), params);
    let twin = id_set(db, &format!("SELECT id FROM twin WHERE {tail}"), params);
    assert_eq!(indexed, twin, "`{tail}` (params={params})");
    indexed
}

#[test]
fn a_query_with_the_same_expression_reads_through_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);

        // The expression as written, and the spellings that are the same expression.
        for spelling in [
            "lower(email) = 'user007@x.io'",
            "LOWER(email) = 'user007@x.io'",
            "( lower( email ) ) = 'user007@x.io'",
            "'user007@x.io' = lower(email)",
            "lower(u.email) = 'user007@x.io'",
        ] {
            let p = plan(&db, &format!("SELECT id FROM u WHERE {spelling}"), params);
            assert!(p.contains("Index Scan using u_lower on u"), "`{spelling}`: {p}");
            let got = id_set(&db, &format!("SELECT id FROM u WHERE {spelling}"), params);
            assert_eq!(got, vec![7], "`{spelling}`");
        }
        assert_eq!(same_as_twin(&db, "lower(email) = 'user008@x.io'", params), vec![8]);
        assert_eq!(
            same_as_twin(&db, "lower(email) = 'User008@X.io'", params),
            Vec::<i64>::new()
        );
        // A range on the expression.
        let p = plan(
            &db,
            "SELECT id FROM u WHERE lower(email) >= 'user190' AND lower(email) < 'user195'",
            params,
        );
        assert!(p.contains("Index Scan using u_lower on u"), "{p}");
        assert_eq!(
            same_as_twin(&db, "lower(email) >= 'user190' AND lower(email) < 'user195'", params).len(),
            5
        );

        // A JSON field.
        let p = plan(&db, "SELECT id FROM u WHERE payload->>'user_id' = 'u7'", params);
        assert!(p.contains("Index Scan using u_uid on u"), "{p}");
        assert_eq!(same_as_twin(&db, "payload->>'user_id' = 'u7'", params).len(), 8);
        assert_eq!(
            same_as_twin(&db, "(payload->>'user_id') = 'u7' AND a = 7", params).len(),
            4
        );
        // Another field of the same document is another expression.
        let p = plan(&db, "SELECT id FROM u WHERE payload->>'n' = '7'", params);
        assert!(!p.contains("u_uid"), "{p}");
        assert_eq!(same_as_twin(&db, "payload->>'n' = '7'", params), vec![7]);

        // Arithmetic.
        let p = plan(&db, "SELECT id FROM u WHERE a + b = 27", params);
        assert!(p.contains("Index Scan using u_sum on u"), "{p}");
        assert_eq!(same_as_twin(&db, "a + b = 27", params), vec![189, 198]);
        assert_eq!(same_as_twin(&db, "a + b >= 27", params), vec![189, 198, 199]);
        // A different expression over the same columns is not this index.
        for tail in [
            "b + a = 27",
            "a - b = 0",
            "a + b + 0 = 27",
            "upper(email) = 'USER007@X.IO'",
        ] {
            let p = plan(&db, &format!("SELECT id FROM u WHERE {tail}"), params);
            assert!(!p.contains("Index Scan"), "`{tail}`: {p}");
            same_as_twin(&db, tail, params);
        }
        // The plain column is not the expression either.
        assert_eq!(same_as_twin(&db, "email = 'User008@X.io'", params), vec![8]);
        assert!(!plan(&db, "SELECT id FROM u WHERE email = 'User008@X.io'", params).contains("u_lower"));
    }
}

#[test]
fn bound_parameters_read_through_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db, true);
    let sql = "SELECT id FROM u WHERE lower(email) = $1";
    let bound = [Value::String("user042@x.io".into())];
    assert!(plan_bound(&db, sql, &bound).contains("Index Scan using u_lower on u"));
    assert_eq!(id_set_bound(&db, sql, &bound), vec![42]);
    assert_eq!(
        id_set_bound(&db, sql, &[Value::String("user043@x.io".into())]),
        vec![43]
    );
    assert_eq!(
        id_set_bound(&db, sql, &[Value::String("nobody".into())]),
        Vec::<i64>::new()
    );
    let sql = "SELECT id FROM u WHERE a + b = $1";
    assert_eq!(id_set_bound(&db, sql, &[Value::Int4(27)]), vec![189, 198]);
    assert_eq!(id_set_bound(&db, sql, &[Value::Int8(28)]), vec![199]);
    // A constant of another kind than the expression's values is answered from
    // the rows, as it was before the index existed.
    let sql = "SELECT id FROM u WHERE payload->>'user_id' = $1";
    assert_eq!(id_set_bound(&db, sql, &[Value::String("u24".into())]).len(), 8);
}

#[test]
fn every_write_keeps_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        for table in ["u", "twin"] {
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (1000, 'NEW@x.io', 20, 7, '{{\"user_id\":\"u7\"}}')"),
                params,
            );
            must(
                &db,
                &format!("UPDATE {table} SET email = 'Moved@X.io' WHERE id = 7"),
                params,
            );
            must(&db, &format!("UPDATE {table} SET a = 0 WHERE id = 189"), params);
            must(
                &db,
                &format!("UPDATE {table} SET payload = '{{\"user_id\":\"u7\"}}' WHERE id = 1"),
                params,
            );
            must(&db, &format!("DELETE FROM {table} WHERE id = 198"), params);
            must(&db, &format!("UPDATE {table} SET email = NULL WHERE id = 9"), params);
        }
        assert_eq!(same_as_twin(&db, "lower(email) = 'new@x.io'", params), vec![1000]);
        assert_eq!(
            same_as_twin(&db, "lower(email) = 'user007@x.io'", params),
            Vec::<i64>::new()
        );
        assert_eq!(same_as_twin(&db, "lower(email) = 'moved@x.io'", params), vec![7]);
        assert_eq!(
            same_as_twin(&db, "lower(email) = 'user009@x.io'", params),
            Vec::<i64>::new()
        );
        assert_eq!(same_as_twin(&db, "a + b = 27", params), vec![1000]);
        assert_eq!(same_as_twin(&db, "a + b = 18", params).len(), 11);
        assert_eq!(same_as_twin(&db, "payload->>'user_id' = 'u7'", params).len(), 10);
        assert!(plan(&db, "SELECT id FROM u WHERE lower(email) = 'moved@x.io'", params)
            .contains("Index Scan using u_lower"));

        // Rolled back: as before. Committed: changed.
        for table in ["u", "twin"] {
            must(&db, "BEGIN", params);
            must(
                &db,
                &format!("UPDATE {table} SET email = 'Gone@x.io' WHERE id = 7"),
                params,
            );
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (2000, 'Temp@x.io', 1, 1, '{{}}')"),
                params,
            );
            must(&db, "ROLLBACK", params);
            must(&db, "BEGIN", params);
            must(
                &db,
                &format!("UPDATE {table} SET email = 'Kept@x.io' WHERE id = 8"),
                params,
            );
            must(&db, "COMMIT", params);
        }
        assert_eq!(same_as_twin(&db, "lower(email) = 'moved@x.io'", params), vec![7]);
        assert_eq!(
            same_as_twin(&db, "lower(email) = 'gone@x.io'", params),
            Vec::<i64>::new()
        );
        assert_eq!(
            same_as_twin(&db, "lower(email) = 'temp@x.io'", params),
            Vec::<i64>::new()
        );
        assert_eq!(same_as_twin(&db, "lower(email) = 'kept@x.io'", params), vec![8]);
    }
}

fn accounts(db: &EmbeddedDatabase, params: bool) {
    must(
        db,
        "CREATE TABLE acct (id INT PRIMARY KEY, email TEXT, tenant INT)",
        params,
    );
    must(db, "CREATE UNIQUE INDEX acct_email_ci ON acct (lower(email))", params);
}

fn is_duplicate(err: &heliosdb_nano::Error, index: &str) -> bool {
    let text = err.to_string();
    text.to_lowercase().contains("unique") && text.contains(index)
}

#[test]
fn a_unique_index_on_an_expression_is_enforced() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        accounts(&db, params);
        must(&db, "INSERT INTO acct VALUES (1, 'Ann@X.io', 1)", params);
        let err = run(&db, "INSERT INTO acct VALUES (2, 'ann@x.IO', 1)", params).unwrap_err();
        assert!(is_duplicate(&err, "acct_email_ci"), "{err}");
        must(&db, "INSERT INTO acct VALUES (2, 'bob@x.io', 1)", params);
        // NULL is never a duplicate.
        must(&db, "INSERT INTO acct VALUES (3, NULL, 1), (4, NULL, 1)", params);
        // UPDATE onto another row's key, in any case.
        let err = run(&db, "UPDATE acct SET email = 'ANN@x.io' WHERE id = 2", params).unwrap_err();
        assert!(is_duplicate(&err, "acct_email_ci"), "{err}");
        // A row may change its own spelling: the key does not move.
        must(&db, "UPDATE acct SET email = 'ANN@X.IO' WHERE id = 1", params);
        // An update that touches no key column is not judged at all.
        must(&db, "UPDATE acct SET tenant = 9 WHERE id = 1", params);
        // Two rows of one statement onto one key.
        let err = run(&db, "UPDATE acct SET email = 'Same@x.io' WHERE id IN (3, 4)", params).unwrap_err();
        assert!(is_duplicate(&err, "acct_email_ci"), "{err}");
        // A multi-row INSERT that repeats a key in itself.
        let err = run(
            &db,
            "INSERT INTO acct VALUES (5, 'Cid@x.io', 1), (6, 'CID@x.io', 1)",
            params,
        )
        .unwrap_err();
        assert!(is_duplicate(&err, "acct_email_ci"), "{err}");
        assert_eq!(id_set(&db, "SELECT id FROM acct", params), vec![1, 2, 3, 4]);
        // Freed by a delete, and by a rolled-back insert.
        must(&db, "DELETE FROM acct WHERE id = 1", params);
        must(&db, "BEGIN", params);
        must(&db, "INSERT INTO acct VALUES (7, 'ann@x.io', 1)", params);
        must(&db, "ROLLBACK", params);
        must(&db, "INSERT INTO acct VALUES (8, 'aNN@x.io', 1)", params);
        must(
            &db,
            "INSERT INTO acct VALUES (9, 'zed@x.io', 1) ON CONFLICT DO NOTHING",
            params,
        );
        must(
            &db,
            "INSERT INTO acct VALUES (10, 'ZED@x.io', 1) ON CONFLICT DO NOTHING",
            params,
        );
        assert_eq!(id_set(&db, "SELECT id FROM acct", params), vec![2, 3, 4, 8, 9]);

        // The query side: one probe.
        let p = plan(&db, "SELECT id FROM acct WHERE lower(email) = 'ann@x.io'", params);
        assert!(p.contains("Index Scan using acct_email_ci on acct"), "{p}");
        assert_eq!(
            id_set(&db, "SELECT id FROM acct WHERE lower(email) = 'ann@x.io'", params),
            vec![8]
        );
        let def = indexdef(&db, "acct_email_ci", params);
        assert!(
            def.starts_with("CREATE UNIQUE INDEX acct_email_ci ON public.acct USING btree (lower(email))"),
            "{def}"
        );
    }
}

#[test]
fn rows_that_already_break_it_refuse_the_index() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        must(
            &db,
            "CREATE TABLE acct (id INT PRIMARY KEY, email TEXT, tenant INT)",
            params,
        );
        must(
            &db,
            "INSERT INTO acct VALUES (1, 'Ann@x.io', 1), (2, 'ANN@x.io', 2)",
            params,
        );
        let err = run(&db, "CREATE UNIQUE INDEX acct_email_ci ON acct (lower(email))", params).unwrap_err();
        assert!(err.to_string().contains("acct_email_ci"), "{err}");
        let left = rows(&db, "SELECT indexname FROM pg_indexes WHERE tablename = 'acct'", params);
        assert!(left.iter().all(|r| text(&r[0]) != "acct_email_ci"), "{left:?}");
        must(&db, "INSERT INTO acct VALUES (3, 'ann@x.io', 3)", params);
        // Unique per tenant: an expression and a column in one key.
        must(
            &db,
            "CREATE UNIQUE INDEX acct_tenant_email ON acct (tenant, lower(email))",
            params,
        );
        must(&db, "INSERT INTO acct VALUES (4, 'ann@x.io', 4)", params);
        let err = run(&db, "INSERT INTO acct VALUES (5, 'aNn@x.io', 4)", params).unwrap_err();
        assert!(is_duplicate(&err, "acct_tenant_email"), "{err}");
        let p = plan(
            &db,
            "SELECT id FROM acct WHERE tenant = 4 AND lower(email) = 'ann@x.io'",
            params,
        );
        assert!(p.contains("Index Scan using acct_tenant_email on acct"), "{p}");
        assert_eq!(
            id_set(
                &db,
                "SELECT id FROM acct WHERE tenant = 4 AND lower(email) = 'ann@x.io'",
                params
            ),
            vec![4]
        );
        // And a partial one over an expression.
        must(
            &db,
            "CREATE UNIQUE INDEX acct_live ON acct (upper(email)) WHERE tenant > 100",
            params,
        );
        must(
            &db,
            "INSERT INTO acct VALUES (6, 'x@x.io', 200), (7, 'X@x.io', 50)",
            params,
        );
        let err = run(&db, "INSERT INTO acct VALUES (8, 'X@X.io', 300)", params).unwrap_err();
        assert!(is_duplicate(&err, "acct_live"), "{err}");
    }
}

#[test]
fn order_by_the_expression_with_a_limit_walks_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        let sql = "SELECT id FROM u ORDER BY lower(email) LIMIT 5";
        let p = plan(&db, sql, params);
        assert!(p.contains("Index Ordered Scan using u_lower on u"), "{p}");
        assert_eq!(ids(&db, sql, params), vec![0, 1, 2, 3, 4]);
        let sql = "SELECT id FROM u ORDER BY lower(email) LIMIT 3 OFFSET 100";
        assert_eq!(ids(&db, sql, params), vec![100, 101, 102]);
        let sql = "SELECT id FROM u WHERE a = 7 ORDER BY lower(email) LIMIT 3";
        assert_eq!(ids(&db, sql, params), vec![7, 17, 27]);
        assert_eq!(
            ids(&db, sql, params),
            ids(
                &db,
                "SELECT id FROM twin WHERE a = 7 ORDER BY lower(email) LIMIT 3",
                params
            )
        );

        // A NULL key is ordered by the sort, where the sort puts it: with one in
        // the table the walk is not taken, and the answer is the twin's.
        for table in ["u", "twin"] {
            must(&db, &format!("UPDATE {table} SET email = NULL WHERE id = 150"), params);
        }
        let sql = "SELECT id FROM u ORDER BY lower(email), id LIMIT 5";
        assert_eq!(
            ids(&db, sql, params),
            ids(&db, "SELECT id FROM twin ORDER BY lower(email), id LIMIT 5", params)
        );
        let sql = "SELECT id FROM u ORDER BY lower(email) LIMIT 5";
        assert_eq!(
            ids(&db, sql, params),
            ids(&db, "SELECT id FROM twin ORDER BY lower(email) LIMIT 5", params)
        );
    }
}

#[test]
fn an_expression_that_is_not_immutable_is_refused() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        must(&db, "CREATE TABLE e (id INT PRIMARY KEY, email TEXT, n INT)", params);
        for sql in [
            "CREATE INDEX e_bad ON e ((n + random()))",
            "CREATE INDEX e_bad ON e ((now()))",
            "CREATE UNIQUE INDEX e_bad ON e ((n * random()))",
            "CREATE INDEX e_bad ON e ((n > (SELECT max(n) FROM e)))",
        ] {
            let err = run(&db, sql, params).unwrap_err();
            assert!(err.to_string().contains("must be marked IMMUTABLE"), "`{sql}`: {err}");
        }
        let err = run(&db, "CREATE INDEX e_bad ON e (lower(nope))", params).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        let left = rows(&db, "SELECT indexname FROM pg_indexes WHERE tablename = 'e'", params);
        assert!(left.iter().all(|r| text(&r[0]) != "e_bad"), "{left:?}");
        // An expression that fails for a row that is already there refuses the index.
        must(&db, "INSERT INTO e VALUES (1, 'a', 0)", params);
        let err = run(&db, "CREATE INDEX e_div ON e ((10 / n))", params).unwrap_err();
        assert!(err.to_string().contains("e_div"), "{err}");
        // An index that is only recorded (no tree behind it) is as it was.
        must(
            &db,
            "CREATE INDEX e_fts ON e USING gin (to_tsvector('simple', email))",
            params,
        );
        assert!(indexdef(&db, "e_fts", params).contains("USING gin"));
        assert!(run(
            &db,
            "CREATE UNIQUE INDEX e_fts_u ON e USING gin (to_tsvector('simple', email))",
            params
        )
        .is_err());
    }
}

#[test]
fn it_is_rebuilt_from_its_definition_at_open() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        build(&db, false);
        accounts(&db, false);
        db.execute("INSERT INTO acct VALUES (1, 'Ann@X.io', 1)").unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    for params in FAMILIES {
        let p = plan(&db, "SELECT id FROM u WHERE lower(email) = 'user007@x.io'", params);
        assert!(p.contains("Index Scan using u_lower on u"), "{p}");
        assert_eq!(same_as_twin(&db, "lower(email) = 'user007@x.io'", params), vec![7]);
        assert_eq!(same_as_twin(&db, "a + b = 27", params), vec![189, 198]);
        assert!(plan(&db, "SELECT id FROM u WHERE a + b = 27", params).contains("Index Scan using u_sum"));
        assert_eq!(same_as_twin(&db, "payload->>'user_id' = 'u7'", params).len(), 8);
        let def = indexdef(&db, "acct_email_ci", params);
        assert!(def.contains("UNIQUE") && def.contains("(lower(email))"), "{def}");
    }
    let err = db.execute("INSERT INTO acct VALUES (2, 'ANN@x.io', 1)").unwrap_err();
    assert!(is_duplicate(&err, "acct_email_ci"), "{err}");
    db.execute("INSERT INTO u VALUES (5000, 'Late@x.io', 1, 1, '{}')")
        .unwrap();
    db.execute("INSERT INTO twin VALUES (5000, 'Late@x.io', 1, 1, '{}')")
        .unwrap();
    assert_eq!(same_as_twin(&db, "lower(email) = 'late@x.io'", false), vec![5000]);
    db.execute("DROP INDEX u_lower").unwrap();
    assert!(!plan(&db, "SELECT id FROM u WHERE lower(email) = 'late@x.io'", false).contains("u_lower"));
    assert_eq!(same_as_twin(&db, "lower(email) = 'late@x.io'", false), vec![5000]);
}

#[test]
fn a_dump_carries_it_unique_and_all() {
    for format in FORMATS {
        let src = EmbeddedDatabase::new_in_memory().unwrap();
        build(&src, false);
        accounts(&src, false);
        src.execute("INSERT INTO acct VALUES (1, 'Ann@X.io', 1)").unwrap();
        let (db, _dir) = round_trip(&src, format);
        let def = indexdef(&db, "acct_email_ci", false);
        assert!(
            def.contains("UNIQUE") && def.contains("(lower(email))"),
            "{format:?}: {def}"
        );
        let err = db.execute("INSERT INTO acct VALUES (2, 'ANN@x.io', 1)").unwrap_err();
        assert!(is_duplicate(&err, "acct_email_ci"), "{format:?}: {err}");
        let p = plan(&db, "SELECT id FROM u WHERE lower(email) = 'user007@x.io'", false);
        assert!(p.contains("Index Scan using u_lower on u"), "{format:?}: {p}");
        assert_eq!(same_as_twin(&db, "lower(email) = 'user007@x.io'", false), vec![7]);
        assert_eq!(same_as_twin(&db, "payload->>'user_id' = 'u7'", false).len(), 8);
    }
}

/// The standby path: a sealed image opens with its expression indexes, and
/// every answer is the rows' answer.
#[test]
fn a_physical_snapshot_opens_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = heliosdb_nano::Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.memory_only = false;
    {
        let db = EmbeddedDatabase::with_config(config.clone()).unwrap();
        build(&db, false);
    }
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config)
        .unwrap_or_else(|e| panic!("hydrating a snapshot with expression indexes: {e}"));
    let db = hydrated.database;
    for params in FAMILIES {
        assert!(indexdef(&db, "u_lower", params).contains("lower(email)"));
        assert_eq!(same_as_twin(&db, "lower(email) = 'user007@x.io'", params), vec![7]);
        assert_eq!(same_as_twin(&db, "a + b = 27", params), vec![189, 198]);
        assert_eq!(same_as_twin(&db, "payload->>'user_id' = 'u7'", params).len(), 8);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_the_postgresql_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (conn, server) = serve(Arc::clone(&db)).await;
    let client = connect(&conn).await;
    for statement in [
        "CREATE TABLE acct (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX acct_email_ci ON acct (lower(email))",
        "INSERT INTO acct VALUES (1, 'Ann@X.io')",
        "INSERT INTO acct VALUES (2, 'Bob@X.io')",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    assert_eq!(
        wire_sqlstate(&client, "INSERT INTO acct VALUES (3, 'ANN@x.io')").await,
        "23505"
    );
    assert_eq!(
        wire_sqlstate(&client, "UPDATE acct SET email = 'ann@x.io' WHERE id = 2").await,
        "23505"
    );
    let err = client
        .execute("INSERT INTO acct VALUES ($1, $2)", &[&4i32, &"bOB@x.io"])
        .await
        .unwrap_err();
    assert_eq!(
        err.code().map(|c| c.code().to_string()).as_deref(),
        Some("23505"),
        "{err}"
    );
    client
        .execute("INSERT INTO acct VALUES ($1, $2)", &[&5i32, &"Cid@x.io"])
        .await
        .unwrap();

    let p: Vec<String> = wire_rows(&client, "EXPLAIN SELECT id FROM acct WHERE lower(email) = 'cid@x.io'")
        .await
        .into_iter()
        .flatten()
        .collect();
    assert!(p.join("\n").contains("Index Scan using acct_email_ci on acct"), "{p:?}");
    let got = client
        .query("SELECT id FROM acct WHERE lower(email) = $1", &[&"cid@x.io"])
        .await
        .unwrap();
    let got: Vec<i32> = got.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![5]);
    let def = wire_rows(
        &client,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'acct_email_ci'",
    )
    .await;
    assert!(
        def[0][0].contains("UNIQUE") && def[0][0].contains("lower(email)"),
        "{def:?}"
    );
    // Not immutable: 42P17.
    assert_eq!(
        wire_sqlstate(&client, "CREATE INDEX acct_rand ON acct ((id + random()))").await,
        "42P17"
    );
    server.abort();
}
