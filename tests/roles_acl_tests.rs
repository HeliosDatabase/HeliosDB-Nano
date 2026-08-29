//! Roles / ACL surface — regression coverage.
//!
//! # NOTHING HERE TESTS ENFORCEMENT, BECAUSE THERE IS NONE
//!
//! HeliosDB Nano persists roles and grants so that introspection tells the
//! truth. It does **not** check a privilege at any read or write path. Every
//! assertion below is about what the CATALOG records and reports. If you are
//! looking for "role X cannot read table Y", it does not exist in this build
//! and no test in this file implies it does.
//!
//! Two things are covered:
//!
//! 1. **The `DROP ROLE` data-loss landmine.** Before the planner gained an
//!    explicit `ObjectType::Role` arm, its `_ => LogicalPlan::DropTable`
//!    fallback swallowed `DROP ROLE analyst` and executed it as
//!    `DROP TABLE analyst` — silently destroying a table that merely shared the
//!    role's name, and reporting success. The load-bearing assertion in those
//!    tests is "the TABLE is still there". The fallback has since been removed
//!    entirely — the planner's `ObjectType` match is exhaustive — so the same
//!    assertions are made here for `DROP INDEX`, which was the second live
//!    instance of the identical class (roadmap §2.1). `DROP INDEX` became a
//!    real drop in 4.21.0; the guard kept here is the data-loss one (the
//!    same-named TABLE survives). Its full behaviour matrix lives in
//!    `tests/drop_index_tests.rs`.
//!
//! 2. **The storage slice**: `CREATE/ALTER/DROP ROLE` as real DDL, `GRANT` /
//!    `REVOKE` storing ACL records instead of vanishing, and the catalog views
//!    (`pg_roles`, `pg_user`, `pg_authid`,
//!    `information_schema.table_privileges` / `role_table_grants`) reporting
//!    them. Before this, GRANT parsed, "succeeded" and discarded everything,
//!    and `pg_roles` invented two all-privilege superusers.
//!
//! Both DML executor families are exercised on every case:
//!   * text family   — `db.execute()`        → `execute_in_transaction_inner`
//!   * params family — `db.execute_params()` → `execute_plan_with_params_inner`
//!                     (the PG extended protocol: psycopg, JDBC, sqlx,
//!                      node-postgres; plus REST/BaaS)
//! A fix that landed in only one family is this repo's signature defect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use tempfile::TempDir;

/// Run `sql` through the requested executor family. Every behavioural test in
/// this file loops over `[false, true]` so a fix that lands in only one family
/// cannot pass.
fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Rows + column names for a catalog query, with a loud failure if the view is
/// not reachable at all (the pre-existing "unknown relation" behaviour for
/// several of these views on the embedded route).
fn view(db: &EmbeddedDatabase, sql: &str) -> (Vec<heliosdb_nano::Tuple>, Vec<String>) {
    db.query_with_columns(sql)
        .unwrap_or_else(|e| panic!("catalog query must be reachable: {sql}\n  got: {e}"))
}

/// The single row of `pg_roles` for `name`, as (column, rendered value) pairs.
fn pg_role_row(db: &EmbeddedDatabase, name: &str) -> Vec<(String, String)> {
    let (rows, cols) = view(db, "SELECT * FROM pg_roles");
    let row = rows
        .iter()
        .find(|r| r.get(1).map(text).as_deref() == Some(name))
        .unwrap_or_else(|| panic!("pg_roles must contain role `{name}`"));
    cols.iter().cloned().zip(row.values.iter().map(text)).collect()
}

fn role_attr(db: &EmbeddedDatabase, name: &str, column: &str) -> String {
    pg_role_row(db, name)
        .into_iter()
        .find(|(c, _)| c == column)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("pg_roles has no column `{column}`"))
}

/// (privilege, is_grantable) pairs recorded for `grantee` on `table`.
fn stored_grants(db: &EmbeddedDatabase, grantee: &str, table: &str) -> Vec<(String, String)> {
    let (rows, _) = view(
        db,
        "SELECT grantee, table_name, privilege_type, is_grantable \
         FROM information_schema.table_privileges",
    );
    let mut out: Vec<(String, String)> = rows
        .iter()
        .filter(|r| text(&r.values[0]) == grantee && text(&r.values[1]) == table)
        .map(|r| (text(&r.values[2]), text(&r.values[3])))
        .collect();
    out.sort();
    out
}

/// Create `analyst` with one row — the table a `DROP ROLE analyst` used to eat.
fn setup_analyst_table(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE analyst (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO analyst VALUES (1, 'keep me')").unwrap();
}

/// Number of rows currently readable from `analyst`. Panics if the table is
/// gone — which is precisely the failure this suite exists to catch, and it
/// fails LOUD rather than silently reporting zero rows.
fn analyst_rows(db: &EmbeddedDatabase) -> usize {
    db.query("SELECT id, name FROM analyst", &[])
        .expect("table `analyst` must still exist after a DROP ROLE")
        .len()
}

fn first_name(db: &EmbeddedDatabase) -> Option<Value> {
    db.query("SELECT name FROM analyst WHERE id = 1", &[])
        .unwrap()
        .first()
        .and_then(|row| row.get(0).cloned())
}

// ---------------------------------------------------------------------------
// THE landmine: DROP ROLE must never touch a table
// ---------------------------------------------------------------------------

/// TEXT FAMILY. `DROP ROLE analyst` must not remove the TABLE `analyst`.
#[test]
fn drop_role_does_not_drop_a_same_named_table_text_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_analyst_table(&db);

    let result = db.execute("DROP ROLE analyst");

    // THE assertion this change exists for: the table survives.
    assert_eq!(analyst_rows(&db), 1, "DROP ROLE must not delete the table `analyst`");
    assert_eq!(first_name(&db), Some(Value::String("keep me".to_string())));

    // And the statement is honest about having dropped nothing.
    let err = result.expect_err("DROP ROLE on a name that is not a persisted role must not silently succeed");
    let message = err.to_string();
    let lower = message.to_lowercase();
    assert!(message.contains("analyst"), "error must name the role: {message}");
    assert!(lower.contains("role"), "error must be about a ROLE: {message}");
    assert!(
        !lower.contains("table") && !lower.contains("relation"),
        "error must not claim anything about a table/relation (it would also mis-map to 42P01): {message}"
    );
}

/// PARAMS FAMILY (PG extended protocol / REST). Identical outcome required.
#[test]
fn drop_role_does_not_drop_a_same_named_table_params_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_analyst_table(&db);

    let result = db.execute_params("DROP ROLE analyst", &[]);

    assert_eq!(
        analyst_rows(&db),
        1,
        "DROP ROLE on the params family must not delete the table `analyst`"
    );
    assert_eq!(first_name(&db), Some(Value::String("keep me".to_string())));

    let err = result.expect_err("params family must reject DROP ROLE exactly like the text family");
    assert!(err.to_string().contains("analyst"), "error must name the role: {err}");
}

/// The two families must not disagree — a divergence here is the defect that
/// keeps recurring in this codebase.
#[test]
fn drop_role_both_families_agree() {
    let text_db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_analyst_table(&text_db);
    let text_result = text_db.execute("DROP ROLE analyst");

    let params_db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_analyst_table(&params_db);
    let params_result = params_db.execute_params("DROP ROLE analyst", &[]);

    assert_eq!(
        text_result.is_err(),
        params_result.is_err(),
        "text family and params family must agree on DROP ROLE"
    );
    assert_eq!(analyst_rows(&text_db), 1);
    assert_eq!(analyst_rows(&params_db), 1);
}

/// `DROP ROLE IF EXISTS` is a genuine no-op: nothing to drop, so success is
/// truthful — but it still must not touch the table.
#[test]
fn drop_role_if_exists_is_a_noop_and_spares_the_table() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);

        let result = if params {
            db.execute_params("DROP ROLE IF EXISTS analyst", &[])
        } else {
            db.execute("DROP ROLE IF EXISTS analyst")
        };

        result.expect("DROP ROLE IF EXISTS must succeed when the role is absent");
        assert_eq!(
            analyst_rows(&db),
            1,
            "DROP ROLE IF EXISTS must not delete the table `analyst` (params = {params})"
        );
    }
}

/// A role that shares no name with any table is still an error without
/// `IF EXISTS` — and, critically, it must NOT be reported as a missing TABLE.
#[test]
fn drop_role_for_unknown_name_errors_about_a_role_not_a_table() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);

        let result = if params {
            db.execute_params("DROP ROLE ghost", &[])
        } else {
            db.execute("DROP ROLE ghost")
        };

        let err = result.expect_err("DROP ROLE on an unknown role must error");
        let lower = err.to_string().to_lowercase();
        assert!(lower.contains("role"), "error must be about a ROLE: {err}");
        assert!(!lower.contains("table"), "error must not mention a table: {err}");
        // Unrelated table untouched.
        assert_eq!(analyst_rows(&db), 1);
    }
}

/// The comma list (`DROP ROLE a, b`) composes through `DropMulti`. No element
/// may fall back to a table drop.
#[test]
fn drop_role_comma_list_does_not_drop_tables() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);
        db.execute("CREATE TABLE auditor (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO auditor VALUES (7)").unwrap();

        let result = if params {
            db.execute_params("DROP ROLE analyst, auditor", &[])
        } else {
            db.execute("DROP ROLE analyst, auditor")
        };
        assert!(
            result.is_err(),
            "no role exists, so the drop must error (params = {params})"
        );

        assert_eq!(analyst_rows(&db), 1, "table `analyst` must survive (params = {params})");
        assert_eq!(
            db.query("SELECT id FROM auditor", &[])
                .expect("table `auditor` must still exist")
                .len(),
            1,
            "table `auditor` must survive (params = {params})"
        );
    }
}

/// `DROP ROLE IF EXISTS a, b` — the silent spelling, the worst variant of the
/// old bug: it dropped both tables and reported success with no error at all.
#[test]
fn drop_role_if_exists_comma_list_does_not_drop_tables() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);
        db.execute("CREATE TABLE auditor (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO auditor VALUES (7)").unwrap();

        let result = if params {
            db.execute_params("DROP ROLE IF EXISTS analyst, auditor", &[])
        } else {
            db.execute("DROP ROLE IF EXISTS analyst, auditor")
        };
        result.expect("IF EXISTS form must succeed as a no-op");

        assert_eq!(analyst_rows(&db), 1, "table `analyst` must survive (params = {params})");
        assert_eq!(
            db.query("SELECT id FROM auditor", &[])
                .expect("table `auditor` must still exist")
                .len(),
            1,
            "table `auditor` must survive (params = {params})"
        );
    }
}

/// A qualified spelling (`DROP ROLE public.analyst`) must not sneak back into
/// the table path through name resolution.
#[test]
fn qualified_drop_role_does_not_drop_a_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_analyst_table(&db);

    let result = db.execute("DROP ROLE public.analyst");
    assert!(result.is_err(), "no role exists, so the drop must error");
    assert_eq!(analyst_rows(&db), 1, "qualified DROP ROLE must not delete the table");
}

// ---------------------------------------------------------------------------
// The SAME landmine class: every non-relation DROP object kind
// ---------------------------------------------------------------------------

/// `DROP INDEX x` used to hit the exact same `_ => LogicalPlan::DropTable`
/// fallback as `DROP ROLE x` and destroy a TABLE called `x`. The fallback is
/// gone: the planner's `ObjectType` match is exhaustive, and from 4.21.0
/// `DROP INDEX` is a real drop that resolves names only in the `meta:index:`
/// namespace, so it can never reach a relation.
///
/// The load-bearing assertion is identical to the role one and does not change
/// with the implementation: the TABLE survives. There is no index called
/// `analyst`, so the statement must ERROR — naming the index, never the table.
///
/// The full DROP INDEX behaviour matrix lives in `tests/drop_index_tests.rs`;
/// what is kept here is the data-loss guard.
#[test]
fn drop_index_does_not_drop_a_same_named_table() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);

        let result = run(&db, "DROP INDEX analyst", params);

        assert_eq!(
            analyst_rows(&db),
            1,
            "DROP INDEX must not delete the table `analyst` (params = {params})"
        );
        assert_eq!(first_name(&db), Some(Value::String("keep me".to_string())));

        let err = result.expect_err("no index named `analyst` exists, so the drop must error");
        let message = err.to_string();
        assert!(
            message.contains("analyst"),
            "error must name the index (params = {params}): {message}"
        );
        assert!(
            message.to_lowercase().contains("index"),
            "error must be about an INDEX, not a table (params = {params}): {message}"
        );
    }
}

/// The silent spelling. `DROP INDEX IF EXISTS analyst` used to drop the TABLE
/// and report success with no error at all. It is now a genuine no-op — which
/// is the *correct* meaning of `IF EXISTS` now that a real drop exists (in
/// 4.20.0 it deliberately still errored, because nothing was dropped either way
/// and a quiet success would have been a lie). The table must be untouched.
#[test]
fn drop_index_if_exists_does_not_drop_a_same_named_table() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);

        run(&db, "DROP INDEX IF EXISTS analyst", params)
            .unwrap_or_else(|e| panic!("IF EXISTS on a missing index must be a no-op (params = {params}): {e}"));

        assert_eq!(
            analyst_rows(&db),
            1,
            "*** DATA LOSS *** DROP INDEX IF EXISTS deleted the table `analyst` (params = {params})"
        );
        assert_eq!(first_name(&db), Some(Value::String("keep me".to_string())));
    }
}

/// The comma list composes through `DropMulti`; no element may fall back to a
/// table drop. Neither name is an index, so the statement errors — and both
/// TABLES must survive.
#[test]
fn drop_index_comma_list_does_not_drop_tables() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);
        db.execute("CREATE TABLE auditor (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO auditor VALUES (7)").unwrap();

        let result = run(&db, "DROP INDEX analyst, auditor", params);
        assert!(
            result.is_err(),
            "neither name is an index, so the comma list must error (params = {params})"
        );

        assert_eq!(analyst_rows(&db), 1, "table `analyst` must survive (params = {params})");
        assert_eq!(
            db.query("SELECT id FROM auditor", &[])
                .expect("table `auditor` must still exist")
                .len(),
            1,
            "table `auditor` must survive (params = {params})"
        );
    }
}

// ---------------------------------------------------------------------------
// Guard: removing the fallback must not have broken DROP TABLE itself
// ---------------------------------------------------------------------------

/// The fix adds explicit arms for every `ObjectType`; `DROP TABLE` must still
/// drop the table on both families, or the fix has overreached.
///
/// "Gone" is asserted by RE-CREATING the table: a plain `CREATE TABLE` over a
/// surviving table errors ("already exists"), so a successful create — and a
/// zero row count afterwards — proves the drop really happened, without
/// depending on how a read of a missing relation is reported.
#[test]
fn drop_table_still_drops_the_table() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup_analyst_table(&db);

        if params {
            db.execute_params("DROP TABLE analyst", &[]).unwrap();
        } else {
            db.execute("DROP TABLE analyst").unwrap();
        }

        db.execute("CREATE TABLE analyst (id INT PRIMARY KEY, name TEXT)")
            .expect("DROP TABLE must still remove the table, so a re-CREATE succeeds");
        assert_eq!(analyst_rows(&db), 0, "recreated table starts empty (params = {params})");
    }
}

// ---------------------------------------------------------------------------
// Storage slice: CREATE / ALTER / DROP ROLE as real DDL
// ---------------------------------------------------------------------------

/// A created role appears in `pg_roles` with its REAL attribute bits — not the
/// all-true fabrication `pg_roles` used to return for two invented superusers.
/// `pg_authid` never emits the stored password.
#[test]
fn create_role_roundtrip() {
    for params_family in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        run(&db, "CREATE ROLE app LOGIN PASSWORD 'sekrit'", params_family)
            .unwrap_or_else(|e| panic!("CREATE ROLE must be real DDL (params = {params_family}): {e}"));

        assert_eq!(role_attr(&db, "app", "rolcanlogin"), "true");
        assert_eq!(
            role_attr(&db, "app", "rolsuper"),
            "false",
            "a plain CREATE ROLE is NOT a superuser — the old view fabricated this"
        );
        assert_eq!(role_attr(&db, "app", "rolinherit"), "true", "INHERIT is the PG default");
        assert_eq!(role_attr(&db, "app", "rolcreatedb"), "false");
        assert_eq!(role_attr(&db, "app", "rolbypassrls"), "false");
        assert_eq!(role_attr(&db, "app", "rolconnlimit"), "-1");

        // The password is masked, never rendered — on pg_roles AND pg_authid.
        assert_eq!(role_attr(&db, "app", "rolpassword"), "********");
        let (rows, cols) = view(&db, "SELECT * FROM pg_authid");
        let pw_idx = cols.iter().position(|c| c == "rolpassword").expect("rolpassword col");
        for row in &rows {
            let rendered = text(&row.values[pw_idx]);
            assert!(
                !rendered.contains("sekrit"),
                "pg_authid must never emit a stored password, got {rendered}"
            );
        }

        // The two virtual built-ins are still listed for compatibility.
        let (rows, _) = view(&db, "SELECT * FROM pg_roles");
        let names: Vec<String> = rows.iter().map(|r| text(&r.values[1])).collect();
        for builtin in ["postgres", "helios"] {
            assert!(
                names.contains(&builtin.to_string()),
                "built-in {builtin} must remain listed"
            );
        }
        assert_eq!(rows.len(), 3, "two built-ins + one created role, got {names:?}");
    }
}

/// `pg_user` is `pg_roles` filtered to login roles — a NOLOGIN role must not
/// show up there.
#[test]
fn pg_user_lists_only_login_roles() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE loginer LOGIN").unwrap();
    db.execute("CREATE ROLE nologiner NOLOGIN").unwrap();

    let (rows, _) = view(&db, "SELECT * FROM pg_user");
    let names: Vec<String> = rows.iter().map(|r| text(&r.values[0])).collect();
    assert!(
        names.contains(&"loginer".to_string()),
        "login role must appear: {names:?}"
    );
    assert!(
        !names.contains(&"nologiner".to_string()),
        "NOLOGIN role must NOT appear in pg_user: {names:?}"
    );
    // And it IS in pg_roles.
    assert_eq!(role_attr(&db, "nologiner", "rolcanlogin"), "false");
}

/// ALTER ROLE changes only the attributes the statement names.
#[test]
fn alter_role_updates_only_named_attributes() {
    for params_family in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        run(&db, "CREATE ROLE app LOGIN", params_family).unwrap();
        assert_eq!(role_attr(&db, "app", "rolcanlogin"), "true");
        assert_eq!(role_attr(&db, "app", "rolcreatedb"), "false");

        run(&db, "ALTER ROLE app WITH CREATEDB", params_family)
            .unwrap_or_else(|e| panic!("ALTER ROLE must work (params = {params_family}): {e}"));
        assert_eq!(role_attr(&db, "app", "rolcreatedb"), "true");
        assert_eq!(
            role_attr(&db, "app", "rolcanlogin"),
            "true",
            "an ALTER that does not mention LOGIN must not clear it"
        );

        run(&db, "ALTER ROLE app WITH NOLOGIN CONNECTION LIMIT 5", params_family).unwrap();
        assert_eq!(role_attr(&db, "app", "rolcanlogin"), "false");
        assert_eq!(role_attr(&db, "app", "rolconnlimit"), "5");
        assert_eq!(role_attr(&db, "app", "rolcreatedb"), "true");
    }
}

/// DROP ROLE removes the role from the catalog on both families.
#[test]
fn drop_role_removes_a_created_role() {
    for params_family in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        run(&db, "CREATE ROLE app", params_family).unwrap();
        assert_eq!(role_attr(&db, "app", "rolname"), "app");

        run(&db, "DROP ROLE app", params_family)
            .unwrap_or_else(|e| panic!("DROP ROLE must remove a real role (params = {params_family}): {e}"));

        let (rows, _) = view(&db, "SELECT * FROM pg_roles");
        let names: Vec<String> = rows.iter().map(|r| text(&r.values[1])).collect();
        assert!(!names.contains(&"app".to_string()), "role must be gone: {names:?}");
    }
}

/// The negative matrix. Every one of these must FAIL LOUD; a silent success is
/// the class of defect this whole slice removes.
#[test]
fn role_ddl_negative_matrix() {
    for params_family in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        run(&db, "CREATE ROLE app", params_family).unwrap();

        // Duplicate.
        let err = run(&db, "CREATE ROLE app", params_family)
            .expect_err("duplicate CREATE ROLE must error")
            .to_string()
            .to_lowercase();
        assert!(err.contains("already exists"), "expected duplicate error, got {err}");
        // …unless IF NOT EXISTS.
        run(&db, "CREATE ROLE IF NOT EXISTS app", params_family)
            .expect("CREATE ROLE IF NOT EXISTS must succeed on an existing role");

        // Missing role.
        let err = run(&db, "ALTER ROLE ghost WITH LOGIN", params_family)
            .expect_err("ALTER ROLE on a missing role must error")
            .to_string()
            .to_lowercase();
        assert!(err.contains("does not exist"), "expected missing-role error, got {err}");
        let err = run(&db, "DROP ROLE ghost", params_family)
            .expect_err("DROP ROLE on a missing role must error")
            .to_string()
            .to_lowercase();
        assert!(err.contains("does not exist"), "expected missing-role error, got {err}");
        run(&db, "DROP ROLE IF EXISTS ghost", params_family).expect("IF EXISTS must be a no-op success");

        // Reserved names cannot be created, altered or dropped.
        for reserved in ["postgres", "helios", "public"] {
            assert!(
                run(&db, &format!("CREATE ROLE {reserved}"), params_family).is_err(),
                "CREATE ROLE {reserved} must be rejected"
            );
            assert!(
                run(&db, &format!("ALTER ROLE {reserved} WITH LOGIN"), params_family).is_err(),
                "ALTER ROLE {reserved} must be rejected"
            );
            assert!(
                run(&db, &format!("DROP ROLE {reserved}"), params_family).is_err(),
                "DROP ROLE {reserved} must be rejected"
            );
        }

        // Membership is NOT silently dropped — it errors.
        assert!(
            run(&db, "CREATE ROLE member IN ROLE app", params_family).is_err(),
            "role membership must error, not be silently discarded"
        );
        // ALTER ROLE … RENAME / SET are unimplemented and say so.
        assert!(run(&db, "ALTER ROLE app RENAME TO app2", params_family).is_err());
        assert!(
            run(&db, "ALTER ROLE app SET search_path TO x", params_family).is_err(),
            "per-role GUCs are unimplemented and must error rather than be dropped"
        );
    }
}

// ---------------------------------------------------------------------------
// GRANT / REVOKE store ACL records instead of vanishing
// ---------------------------------------------------------------------------

fn db_with_role_and_table(params_family: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(&db, "CREATE ROLE app", params_family).unwrap();
    db
}

/// The headline: a GRANT is STORED and reported, on both families. It is still
/// not enforced — this asserts the catalog, not access control.
#[test]
fn grant_is_stored_and_reported_on_both_families() {
    for params_family in [false, true] {
        let db = db_with_role_and_table(params_family);
        run(&db, "GRANT SELECT ON orders TO app", params_family)
            .unwrap_or_else(|e| panic!("GRANT must be stored (params = {params_family}): {e}"));

        assert_eq!(
            stored_grants(&db, "app", "orders"),
            vec![("SELECT".to_string(), "NO".to_string())],
            "the GRANT must be visible in information_schema.table_privileges (params = {params_family})"
        );

        // role_table_grants mirrors table_privileges (no session identity to
        // filter by — documented in sql::acl_views).
        let (mirror, _) = view(
            &db,
            "SELECT grantee, table_name, privilege_type FROM information_schema.role_table_grants",
        );
        assert_eq!(mirror.len(), 1, "role_table_grants must mirror table_privileges");
    }
}

/// GRANT merges, REVOKE removes exactly what it names, and REVOKE ALL clears
/// the record.
#[test]
fn grant_merge_revoke_partial() {
    for params_family in [false, true] {
        let db = db_with_role_and_table(params_family);

        run(&db, "GRANT SELECT ON orders TO app", params_family).unwrap();
        run(&db, "GRANT INSERT ON orders TO app", params_family).unwrap();
        assert_eq!(
            stored_grants(&db, "app", "orders"),
            vec![
                ("INSERT".to_string(), "NO".to_string()),
                ("SELECT".to_string(), "NO".to_string())
            ],
            "a second GRANT must merge into the same record"
        );

        run(&db, "REVOKE SELECT ON orders FROM app", params_family).unwrap();
        assert_eq!(
            stored_grants(&db, "app", "orders"),
            vec![("INSERT".to_string(), "NO".to_string())],
            "REVOKE must remove exactly the named privilege"
        );

        // Revoking something never granted succeeds silently (PG warns).
        run(&db, "REVOKE DELETE ON orders FROM app", params_family)
            .expect("REVOKE of a never-granted privilege must succeed");

        run(&db, "REVOKE ALL ON orders FROM app", params_family).unwrap();
        assert!(
            stored_grants(&db, "app", "orders").is_empty(),
            "REVOKE ALL must clear the record"
        );
    }
}

/// `GRANT ALL PRIVILEGES` expands to PostgreSQL's seven table privileges, and
/// `WITH GRANT OPTION` is recorded as `is_grantable = YES`.
#[test]
fn grant_all_expands_and_grant_option_is_recorded() {
    for params_family in [false, true] {
        let db = db_with_role_and_table(params_family);
        run(
            &db,
            "GRANT ALL PRIVILEGES ON orders TO app WITH GRANT OPTION",
            params_family,
        )
        .unwrap();

        let stored = stored_grants(&db, "app", "orders");
        let names: Vec<String> = stored.iter().map(|(p, _)| p.clone()).collect();
        let mut expected = vec![
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(names, expected, "GRANT ALL must expand to the seven table privileges");
        assert!(
            stored.iter().all(|(_, grantable)| grantable == "YES"),
            "WITH GRANT OPTION must record is_grantable = YES, got {stored:?}"
        );
    }
}

/// The `public` pseudo-role is a valid grantee without being a created role.
#[test]
fn public_pseudo_role_is_a_valid_grantee() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT SELECT ON orders TO public")
        .expect("public is the SQL-standard pseudo-role and must be accepted");
    assert_eq!(
        stored_grants(&db, "public", "orders"),
        vec![("SELECT".to_string(), "NO".to_string())]
    );
}

/// GRANT/REVOKE negative matrix. Under the default (strict) config these are
/// errors, not silent successes.
#[test]
fn grant_negative_matrix() {
    for params_family in [false, true] {
        let db = db_with_role_and_table(params_family);

        let err = run(&db, "GRANT SELECT ON orders TO ghost", params_family)
            .expect_err("GRANT to an unknown role must error under the default config")
            .to_string()
            .to_lowercase();
        assert!(err.contains("role") && err.contains("does not exist"), "got {err}");

        let err = run(&db, "GRANT SELECT ON no_such_table TO app", params_family)
            .expect_err("GRANT on an unknown table must error")
            .to_string()
            .to_lowercase();
        assert!(err.contains("does not exist"), "got {err}");

        // Unmodelled shapes are refused rather than half-stored.
        assert!(
            run(&db, "GRANT SELECT ON ALL TABLES IN SCHEMA public TO app", params_family).is_err(),
            "ALL TABLES IN SCHEMA is unimplemented and must error"
        );
        assert!(
            run(&db, "GRANT USAGE ON SCHEMA public TO app", params_family).is_err(),
            "schema grants are unimplemented and must error"
        );
        assert!(
            run(&db, "GRANT SELECT (id) ON orders TO app", params_family).is_err(),
            "column-level grants are unimplemented and must error"
        );
        assert!(
            run(&db, "GRANT USAGE ON orders TO app", params_family).is_err(),
            "USAGE is not a table privilege"
        );

        // Nothing above may have left a record behind.
        assert!(
            stored_grants(&db, "app", "orders").is_empty(),
            "a rejected GRANT must store nothing"
        );
    }
}

/// A role that holds grants cannot be dropped; after REVOKE it can, and the
/// privilege row disappears with it.
#[test]
fn drop_role_with_grants() {
    for params_family in [false, true] {
        let db = db_with_role_and_table(params_family);
        run(&db, "GRANT SELECT ON orders TO app", params_family).unwrap();

        let err = run(&db, "DROP ROLE app", params_family)
            .expect_err("a role holding grants must not be droppable")
            .to_string()
            .to_lowercase();
        assert!(
            err.contains("depend"),
            "expected a dependency error mentioning dependent objects, got {err}"
        );

        run(&db, "REVOKE SELECT ON orders FROM app", params_family).unwrap();
        run(&db, "DROP ROLE app", params_family).expect("after REVOKE the role must drop");
        assert!(stored_grants(&db, "app", "orders").is_empty());
    }
}

// ---------------------------------------------------------------------------
// CREATE / ALTER / DROP USER — the spelling sqlparser cannot parse
// ---------------------------------------------------------------------------

#[test]
fn create_user_rewrite() {
    for params_family in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        // PostgreSQL: CREATE USER == CREATE ROLE … LOGIN.
        run(&db, "CREATE USER u PASSWORD 'p'", params_family)
            .unwrap_or_else(|e| panic!("CREATE USER must work (params = {params_family}): {e}"));
        assert_eq!(
            role_attr(&db, "u", "rolcanlogin"),
            "true",
            "CREATE USER defaults to LOGIN"
        );
        assert_eq!(role_attr(&db, "u", "rolpassword"), "********");

        // An explicit NOLOGIN is honoured, not overridden by the default.
        run(&db, "CREATE USER v NOLOGIN", params_family).unwrap();
        assert_eq!(role_attr(&db, "v", "rolcanlogin"), "false");

        run(&db, "ALTER USER u WITH NOLOGIN", params_family).unwrap();
        assert_eq!(role_attr(&db, "u", "rolcanlogin"), "false");

        run(&db, "DROP USER u", params_family).unwrap();
        let (rows, _) = view(&db, "SELECT * FROM pg_roles");
        let names: Vec<String> = rows.iter().map(|r| text(&r.values[1])).collect();
        assert!(
            !names.contains(&"u".to_string()),
            "DROP USER must remove the role: {names:?}"
        );
    }
}

/// The rewrite is head-anchored: ordinary SQL that merely mentions `user` must
/// be untouched.
#[test]
fn user_rewrite_does_not_touch_unrelated_sql() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE user_roles (id INT PRIMARY KEY, note TEXT)")
        .expect("a table named user_roles must still be creatable");
    db.execute("INSERT INTO user_roles VALUES (1, 'drop user impostor')")
        .unwrap();
    assert_eq!(db.query("SELECT id FROM user_roles", &[]).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Catalog reachability — the embedded/wire divergence this slice closes
// ---------------------------------------------------------------------------

/// Every privilege/role view must RESOLVE on the embedded route. Several of
/// them used to fail as unknown relations here while answering (empty) on the
/// PG wire.
#[test]
fn embedded_view_registration() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for name in [
        "pg_roles",
        "pg_user",
        "pg_authid",
        "information_schema.table_privileges",
        "information_schema.role_table_grants",
        "information_schema.column_privileges",
        "information_schema.role_column_grants",
        "information_schema.usage_privileges",
        "information_schema.role_usage_grants",
        "information_schema.role_routine_grants",
        "information_schema.applicable_roles",
        "information_schema.enabled_roles",
        "information_schema.administrable_role_authorizations",
    ] {
        let (_rows, cols) = view(&db, &format!("SELECT * FROM {name}"));
        assert!(!cols.is_empty(), "{name} must report its column list");
    }

    // The eight unpopulated ones are EMPTY, and that is the truthful answer:
    // no column grants and no role membership can exist in this build.
    for name in [
        "information_schema.column_privileges",
        "information_schema.role_column_grants",
        "information_schema.usage_privileges",
        "information_schema.role_usage_grants",
        "information_schema.role_routine_grants",
        "information_schema.applicable_roles",
        "information_schema.enabled_roles",
        "information_schema.administrable_role_authorizations",
    ] {
        let (rows, _) = view(&db, &format!("SELECT * FROM {name}"));
        assert!(rows.is_empty(), "{name} must be empty");
    }
}

/// A grant on a table in a non-`public` schema splits correctly in the view.
#[test]
fn grants_report_the_real_schema() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SCHEMA sales").unwrap();
    db.execute("CREATE TABLE sales.orders (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE ROLE app").unwrap();
    db.execute("GRANT SELECT ON sales.orders TO app").unwrap();

    let (rows, _) = view(
        &db,
        "SELECT table_schema, table_name, grantor FROM information_schema.table_privileges",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(text(&rows[0].values[0]), "sales");
    assert_eq!(text(&rows[0].values[1]), "orders");
    assert_eq!(
        text(&rows[0].values[2]),
        "helios",
        "grantor is the constant built-in until session identity lands"
    );
}

// ---------------------------------------------------------------------------
// Persistence across restart
// ---------------------------------------------------------------------------

/// Roles, their attribute bits, their OIDs and their ACL records must survive
/// closing and reopening the same data directory. Nothing else in this slice
/// matters if the catalog is volatile.
#[test]
fn persistence_restart() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("roles_db");

    let oid_before;
    {
        let db = EmbeddedDatabase::new(&path).unwrap();
        db.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE ROLE app LOGIN CREATEDB CONNECTION LIMIT 7").unwrap();
        db.execute("GRANT SELECT, INSERT ON orders TO app WITH GRANT OPTION")
            .unwrap();
        oid_before = role_attr(&db, "app", "oid");
        assert_ne!(oid_before, "0", "a created role must get a real OID");
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen the same data dir");
    assert_eq!(role_attr(&db, "app", "rolcanlogin"), "true");
    assert_eq!(role_attr(&db, "app", "rolcreatedb"), "true");
    assert_eq!(role_attr(&db, "app", "rolconnlimit"), "7");
    assert_eq!(role_attr(&db, "app", "oid"), oid_before, "OIDs must be stable");
    assert_eq!(
        stored_grants(&db, "app", "orders"),
        vec![
            ("INSERT".to_string(), "YES".to_string()),
            ("SELECT".to_string(), "YES".to_string())
        ],
        "stored grants must survive a restart"
    );

    // A second created role must not reuse the first one's OID.
    db.execute("CREATE ROLE other").unwrap();
    assert_ne!(role_attr(&db, "other", "oid"), oid_before, "OIDs must not be reused");
}

// ---------------------------------------------------------------------------
// The one new tunable
// ---------------------------------------------------------------------------

/// `[authentication] legacy_acl_noop = true` restores the pre-4.20 leniency:
/// a GRANT naming an unknown role succeeds as a no-op instead of erroring.
/// It does NOT restore "store nothing", and it changes no enforcement (there
/// is none either way).
#[test]
fn legacy_acl_noop_flag() {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.authentication.legacy_acl_noop = true;
    let lenient = EmbeddedDatabase::with_config(config).unwrap();
    lenient.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    lenient.execute("CREATE ROLE app").unwrap();

    lenient
        .execute("GRANT SELECT ON orders TO ghost")
        .expect("legacy_acl_noop must accept an unknown grantee");
    assert!(
        stored_grants(&lenient, "ghost", "orders").is_empty(),
        "a skipped grantee must still store nothing"
    );
    // A well-formed grant is STILL stored under the lenient flag.
    lenient.execute("GRANT SELECT ON orders TO app").unwrap();
    assert_eq!(
        stored_grants(&lenient, "app", "orders"),
        vec![("SELECT".to_string(), "NO".to_string())]
    );

    // Default config is strict.
    let strict = EmbeddedDatabase::new_in_memory().unwrap();
    strict.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    assert!(
        strict.execute("GRANT SELECT ON orders TO ghost").is_err(),
        "the default config must reject an unknown grantee"
    );
}

// ---------------------------------------------------------------------------
// TDE: the role/ACL readers must go through the decrypting read path
// ---------------------------------------------------------------------------

/// `save_role` / `grant_privileges` write through `StorageEngine::put`, which
/// ENCRYPTS when a key manager is configured. `list_roles` / `list_acls` used to
/// read values straight off the raw RocksDB iterator, which on a TDE data dir
/// hands back CIPHERTEXT — so every `pg_roles` / `pg_user` / `pg_authid` /
/// `information_schema.table_privileges` read, psql `\du`, MySQL `SHOW GRANTS`
/// and `DROP ROLE`'s dependency check hard-errored with *role catalog record is
/// not in a recognised format* the moment one role existed. They now read
/// through `StorageEngine::meta_blobs_with_prefix`, i.e. through `get`, which is
/// the one place decryption happens.
///
/// Gated on the `encryption` feature only because a key manager cannot be
/// constructed without it. `encryption` is in the DEFAULT feature set, so this
/// runs in the standard gate — it is not an opt-in test.
#[cfg(feature = "encryption")]
#[test]
fn roles_and_grants_are_readable_on_an_encrypted_database() {
    // A per-test env var name: `cargo test` runs these in ONE process, and
    // env vars are process-global.
    const KEY_VAR: &str = "HELIOSDB_TEST_ROLES_ACL_TDE_KEY";
    // 32 bytes of key material, hex-encoded (64 chars).
    std::env::set_var(
        KEY_VAR,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );

    let mut config = Config::default();
    config.storage.memory_only = true;
    config.encryption.enabled = true;
    config.encryption.key_source = heliosdb_nano::KeySource::Environment(KEY_VAR.to_string());
    let db = EmbeddedDatabase::with_config(config).expect("encrypted in-memory database");

    db.execute("CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    // `LOGIN` so the role also shows up in `pg_user`, which filters on it.
    db.execute("CREATE ROLE analyst_tde LOGIN").unwrap();
    db.execute("GRANT SELECT ON orders TO analyst_tde").unwrap();

    // Each of these routes through `list_roles` / `list_acls`.
    assert_eq!(role_attr(&db, "analyst_tde", "rolname"), "analyst_tde");
    let (rows, _) = view(&db, "SELECT * FROM pg_user");
    assert!(
        rows.iter().any(|r| text(&r.values[0]) == "analyst_tde"),
        "pg_user must list the role on an encrypted data dir"
    );
    assert_eq!(
        stored_grants(&db, "analyst_tde", "orders"),
        vec![("SELECT".to_string(), "NO".to_string())],
        "the stored grant must be readable on an encrypted data dir"
    );

    // DROP ROLE's dependency check also reads the ACL catalog.
    assert!(
        db.execute("DROP ROLE analyst_tde").is_err(),
        "a role that still holds grants must be refused, not fail to decode"
    );
    db.execute("REVOKE SELECT ON orders FROM analyst_tde").unwrap();
    db.execute("DROP ROLE analyst_tde")
        .expect("DROP ROLE must succeed once the grant is revoked");

    std::env::remove_var(KEY_VAR);
}

// ---------------------------------------------------------------------------
// bincode discriminant guard
// ---------------------------------------------------------------------------

/// `LogicalPlan` is bincode-persisted for materialized-view query plans, and
/// bincode encodes enum variants POSITIONALLY. A new variant inserted anywhere
/// but the END shifts every later variant's on-disk discriminant and silently
/// corrupts every MV plan written by an older binary — the single
/// highest-severity trap in this change.
///
/// This asserts the invariant directly on the encoding rather than through a
/// live materialized view: the four variants added by this slice must all sort
/// AFTER `Noop`, `CreateTableAs` and `DropRole`, and after each other in
/// declaration order.
///
/// EXTEND THIS ARRAY WITH EVERY NEW `LogicalPlan` VARIANT. It is the repo's
/// only mechanical check that an append was an append; a variant left out of it
/// is a variant whose position nothing verifies. `DropIndex` (v4.21.0) is the
/// most recent addition.
#[test]
fn new_plan_variants_were_appended_not_inserted() {
    use heliosdb_nano::sql::LogicalPlan;

    fn discriminant(plan: &LogicalPlan) -> u32 {
        let bytes = bincode::serialize(plan).expect("LogicalPlan must bincode-serialize");
        assert!(bytes.len() >= 4, "bincode encodes the variant index as 4 bytes");
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    let noop = LogicalPlan::Noop;
    let ctas = LogicalPlan::CreateTableAs {
        name: "t".to_string(),
        column_names: Vec::new(),
        if_not_exists: false,
        query: Box::new(LogicalPlan::Noop),
        with_data: true,
    };
    let drop_role = LogicalPlan::DropRole {
        name: "r".to_string(),
        if_exists: false,
    };
    let create_role = LogicalPlan::CreateRole {
        name: "r".to_string(),
        if_not_exists: false,
        login: false,
        superuser: false,
        createdb: false,
        createrole: false,
        inherit: true,
        replication: false,
        bypassrls: false,
        conn_limit: None,
        valid_until: None,
        password: None,
    };
    let alter_role = LogicalPlan::AlterRole {
        name: "r".to_string(),
        set_login: None,
        set_superuser: None,
        set_createdb: None,
        set_createrole: None,
        set_inherit: None,
        set_replication: None,
        set_bypassrls: None,
        set_conn_limit: None,
        set_valid_until: None,
        set_password: None,
    };
    let grant = LogicalPlan::GrantPrivileges {
        privileges: vec!["SELECT".to_string()],
        all_privileges: false,
        object_type: "table".to_string(),
        objects: vec!["t".to_string()],
        grantees: vec!["r".to_string()],
        with_grant_option: false,
    };
    let revoke = LogicalPlan::RevokePrivileges {
        privileges: vec!["SELECT".to_string()],
        all_privileges: false,
        object_type: "table".to_string(),
        objects: vec!["t".to_string()],
        grantees: vec!["r".to_string()],
    };

    // Appended by the v4.21.0 DROP INDEX slice, AFTER `RevokePrivileges` — the
    // variant that was last when this guard was written. Listed here because a
    // guard that is not extended with the variant it was written to protect
    // guards nothing: without this row, nothing asserts that `DropIndex` was
    // appended rather than inserted.
    let drop_index = LogicalPlan::DropIndex {
        name: "i".to_string(),
        if_exists: false,
    };

    let ordered = [
        ("Noop", discriminant(&noop)),
        ("CreateTableAs", discriminant(&ctas)),
        ("DropRole", discriminant(&drop_role)),
        ("CreateRole", discriminant(&create_role)),
        ("AlterRole", discriminant(&alter_role)),
        ("GrantPrivileges", discriminant(&grant)),
        ("RevokePrivileges", discriminant(&revoke)),
        ("DropIndex", discriminant(&drop_index)),
    ];
    for pair in ordered.windows(2) {
        let (before_name, before) = pair[0];
        let (after_name, after) = pair[1];
        assert!(
            after > before,
            "`{after_name}` must be declared AFTER `{before_name}` (bincode discriminants are \
             positional and LogicalPlan is persisted): {after_name}={after}, {before_name}={before}"
        );
    }
}

/// A round-trip through the persisted encoding: an older binary's bytes for a
/// pre-existing variant must still decode to that same variant after the
/// append. `Noop` stands in for "any plan written before this change".
#[test]
fn existing_plan_variants_still_decode_after_the_append() {
    use heliosdb_nano::sql::LogicalPlan;
    let encoded = bincode::serialize(&LogicalPlan::Noop).unwrap();
    let decoded: LogicalPlan = bincode::deserialize(&encoded).expect("persisted plan must still decode");
    assert!(
        matches!(decoded, LogicalPlan::Noop),
        "a persisted Noop must not decode as a different variant, got {decoded:?}"
    );
}
