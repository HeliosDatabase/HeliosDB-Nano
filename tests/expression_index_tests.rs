//! GH#34: `CREATE INDEX` on an expression.
//!
//! `CREATE INDEX t_c ON mem0 ((payload->>'user_id'))` and
//! `CREATE INDEX t_d ON mem0 USING gin (to_tsvector('simple', payload->>'x'))`
//! used to fail with "Column name expected in CREATE INDEX", which stopped
//! mem0's pgvector store from initialising (it issues the statement
//! unconditionally).
//!
//! The index is accepted and recorded (persisted, listed in `pg_indexes`,
//! dropped by `DROP INDEX` / `DROP TABLE`, kept across a reopen). One over a
//! btree is also kept and read through, and may be UNIQUE: see
//! `tests/index_expression.rs`. One `USING gin` is still only recorded, and a
//! UNIQUE one there is refused, because nothing would check it.
//!
//! Every case runs through both executor families: `db.execute()` /
//! `db.query()` and `db.execute_params()` / `db.query_params()`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn run(db: &EmbeddedDatabase, sql: &str, params: bool) -> heliosdb_nano::Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn rows(db: &EmbeddedDatabase, sql: &str, params: bool) -> Vec<Vec<Value>> {
    let tuples = if params {
        db.query_params(sql, &[]).unwrap()
    } else {
        db.query(sql, &[]).unwrap()
    };
    tuples.into_iter().map(|t| t.values).collect()
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn index_defs(db: &EmbeddedDatabase, table: &str, params: bool) -> Vec<(String, String)> {
    rows(
        db,
        &format!("SELECT indexname, indexdef FROM pg_indexes WHERE tablename = '{table}'"),
        params,
    )
    .iter()
    .map(|r| (text(&r[0]), text(&r[1])))
    .collect()
}

fn setup(db: &EmbeddedDatabase, params: bool) {
    run(
        db,
        "CREATE TABLE mem0 (id INT PRIMARY KEY, email TEXT, payload JSONB)",
        params,
    )
    .unwrap();
    run(
        db,
        "INSERT INTO mem0 VALUES (1, 'A@x.io', '{\"user_id\":\"u1\",\"x\":\"hello world\"}'), \
         (2, 'b@x.io', '{\"user_id\":\"u2\",\"x\":\"bye\"}')",
        params,
    )
    .unwrap();
}

#[test]
fn the_mem0_statements_are_accepted_and_listed() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, params);
        run(&db, "CREATE INDEX t_c ON mem0 ((payload->>'user_id'))", params).unwrap();
        run(
            &db,
            "CREATE INDEX t_d ON mem0 USING gin(to_tsvector('simple', payload->>'x'))",
            params,
        )
        .unwrap();
        run(&db, "CREATE INDEX t_e ON mem0 (lower(email))", params).unwrap();
        // mem0 re-runs its init: IF NOT EXISTS must be a no-op.
        run(
            &db,
            "CREATE INDEX IF NOT EXISTS t_c ON mem0 ((payload->>'user_id'))",
            params,
        )
        .unwrap();
        assert!(
            run(&db, "CREATE INDEX t_c ON mem0 ((payload->>'user_id'))", params).is_err(),
            "a taken name is an error without IF NOT EXISTS (params={params})"
        );

        let defs = index_defs(&db, "mem0", params);
        let def = |name: &str| {
            defs.iter()
                .find(|(n, _)| n == name)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| panic!("{name} missing from pg_indexes: {defs:?} (params={params})"))
        };
        let c = def("t_c");
        assert!(c.contains("USING btree") && c.contains("user_id"), "{c}");
        let d = def("t_d");
        assert!(d.contains("USING gin") && d.contains("to_tsvector"), "{d}");
        let e = def("t_e");
        assert!(e.contains("USING btree") && e.contains("lower(email)"), "{e}");
    }
}

#[test]
fn queries_stay_correct_through_writes() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, params);
        run(&db, "CREATE INDEX t_c ON mem0 ((payload->>'user_id'))", params).unwrap();
        run(&db, "CREATE INDEX t_e ON mem0 (lower(email))", params).unwrap();

        run(
            &db,
            "INSERT INTO mem0 VALUES (3, 'C@x.io', '{\"user_id\":\"u1\"}')",
            params,
        )
        .unwrap();
        run(
            &db,
            "UPDATE mem0 SET payload = '{\"user_id\":\"u9\"}' WHERE id = 1",
            params,
        )
        .unwrap();
        run(&db, "DELETE FROM mem0 WHERE id = 2", params).unwrap();

        let got = rows(
            &db,
            "SELECT id FROM mem0 WHERE payload->>'user_id' = 'u1' ORDER BY id",
            params,
        );
        assert_eq!(got, vec![vec![Value::Int4(3)]], "params={params}");
        let got = rows(&db, "SELECT id FROM mem0 WHERE lower(email) = 'a@x.io'", params);
        assert_eq!(got, vec![vec![Value::Int4(1)]], "params={params}");
        let got = rows(&db, "SELECT id FROM mem0 WHERE payload->>'user_id' = 'u2'", params);
        assert!(got.is_empty(), "params={params}: {got:?}");
    }
}

#[test]
fn drop_index_and_drop_table_remove_it() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, params);
        run(&db, "CREATE INDEX t_c ON mem0 ((payload->>'user_id'))", params).unwrap();
        run(&db, "CREATE INDEX t_e ON mem0 (lower(email))", params).unwrap();
        run(&db, "DROP INDEX t_c", params).unwrap();
        let names: Vec<String> = index_defs(&db, "mem0", params).into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"t_c".to_string()), "{names:?}");
        assert!(names.contains(&"t_e".to_string()), "{names:?}");
        // The name is free again.
        run(&db, "CREATE INDEX t_c ON mem0 ((payload->>'user_id'))", params).unwrap();

        run(&db, "DROP TABLE mem0", params).unwrap();
        setup(&db, params);
        assert!(index_defs(&db, "mem0", params)
            .iter()
            .all(|(n, _)| n != "t_c" && n != "t_e"));
        run(&db, "CREATE INDEX t_e ON mem0 (lower(email))", params).unwrap();
    }
}

#[test]
fn unsupported_shapes_fail_explicitly() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, params);
        // A UNIQUE index over an expression is supported on a btree since 5.0
        // (`tests/index_expression.rs`); what has no tree to check it is refused.
        let err = run(
            &db,
            "CREATE UNIQUE INDEX u ON mem0 USING gin (to_tsvector('simple', email))",
            params,
        )
        .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
        let err = run(&db, "CREATE INDEX bad ON mem0 (lower(nope))", params).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        // Refused statements leave nothing behind.
        assert!(index_defs(&db, "mem0", params)
            .iter()
            .all(|(n, _)| n != "u" && n != "bad"));
        // A unique index that is refused must not have started enforcing anything.
        run(&db, "INSERT INTO mem0 VALUES (5, 'a@x.io', '{}')", params).unwrap();
    }
}

#[test]
fn the_definition_survives_a_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "nano_expr_idx_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        setup(&db, false);
        db.execute("CREATE INDEX t_c ON mem0 ((payload->>'user_id'))").unwrap();
    }
    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        for params in [false, true] {
            assert!(
                index_defs(&db, "mem0", params).iter().any(|(n, _)| n == "t_c"),
                "params={params}"
            );
            let got = rows(&db, "SELECT id FROM mem0 WHERE payload->>'user_id' = 'u2'", params);
            assert_eq!(got, vec![vec![Value::Int4(2)]]);
        }
        db.execute("DROP INDEX t_c").unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sealed image of a database with an expression index hydrates as a
/// physical snapshot (the standby path): the definition is kept and listed.
#[test]
fn a_physical_snapshot_with_an_expression_index_hydrates() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = heliosdb_nano::Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.memory_only = false;
    {
        let db = EmbeddedDatabase::with_config(config.clone()).unwrap();
        setup(&db, false);
        db.execute("CREATE INDEX t_c ON mem0 ((payload->>'user_id'))").unwrap();
        db.execute("CREATE INDEX t_l ON mem0 (lower(payload->>'user_id'))")
            .unwrap();
    }
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config)
        .unwrap_or_else(|e| panic!("hydrating a snapshot with an expression index: {e}"));
    let db = hydrated.database;
    for params in [false, true] {
        let names: Vec<String> = index_defs(&db, "mem0", params).into_iter().map(|(n, _)| n).collect();
        assert!(
            names.contains(&"t_c".to_string()) && names.contains(&"t_l".to_string()),
            "{names:?}"
        );
        let got = rows(&db, "SELECT id FROM mem0 WHERE payload->>'user_id' = 'u2'", params);
        assert_eq!(got, vec![vec![Value::Int4(2)]]);
    }
}

/// DROP COLUMN drops an expression index that reads the column (PostgreSQL's
/// rule); RENAME COLUMN of such a column is refused rather than leaving the
/// index's key text naming a column the table no longer has.
#[test]
fn drop_and_rename_column_under_an_expression_index() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, params);
        run(&db, "CREATE INDEX t_low ON mem0 (lower(email))", params).unwrap();
        run(&db, "CREATE INDEX t_uid ON mem0 ((payload->>'user_id'))", params).unwrap();

        let err = run(&db, "ALTER TABLE mem0 RENAME COLUMN email TO mail", params)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("t_low") && err.contains("is not supported"),
            "params={params}: {err}"
        );
        // Nothing changed: the column keeps its name, the index is listed.
        assert_eq!(
            rows(&db, "SELECT id FROM mem0 WHERE lower(email) = 'a@x.io'", params),
            vec![vec![Value::Int4(1)]]
        );
        assert!(index_defs(&db, "mem0", params).iter().any(|(n, _)| n == "t_low"));
        // A column no expression index reads still renames.
        run(&db, "ALTER TABLE mem0 RENAME COLUMN id TO ident", params).unwrap();

        run(&db, "ALTER TABLE mem0 DROP COLUMN email", params).unwrap();
        let names: Vec<String> = index_defs(&db, "mem0", params).into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"t_low".to_string()), "params={params}: {names:?}");
        assert!(names.contains(&"t_uid".to_string()), "params={params}: {names:?}");
        // The name is free again, and a new index over a new column works.
        run(&db, "ALTER TABLE mem0 ADD COLUMN email TEXT", params).unwrap();
        run(&db, "CREATE INDEX t_low ON mem0 (lower(email))", params).unwrap();
    }
}
