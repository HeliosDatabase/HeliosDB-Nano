//! GRANT and REVOKE are enforced: a statement runs only if the session's role
//! may run it.
//!
//! Every case here drives a session whose login names a role in the catalog,
//! through each statement route a session has: the text family
//! (`execute_for_session` / `query_with_columns_for_session`,
//! `execute_in_session` / `query_in_session`) and the params family
//! (`execute_params_for_session` / `query_params_for_session`). A check that
//! landed on one route only cannot pass.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use tempfile::TempDir;

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn login(db: &EmbeddedDatabase, role: &str) -> SessionId {
    db.create_session(role, IsolationLevel::ReadCommitted)
        .unwrap_or_else(|e| panic!("session for {role}: {e}"))
}

/// The statement routes that return a row count.
const WRITE_ROUTES: [&str; 3] = ["execute_for_session", "execute_in_session", "execute_params_for_session"];
/// The statement routes that return rows.
const READ_ROUTES: [&str; 3] = [
    "query_with_columns_for_session",
    "query_in_session",
    "query_params_for_session",
];

fn write(db: &EmbeddedDatabase, session: SessionId, route: &str, sql: &str) -> Result<u64, Error> {
    match route {
        "execute_for_session" => db.execute_for_session(session, sql),
        "execute_in_session" => db.execute_in_session(session, sql),
        "execute_params_for_session" => db.execute_params_for_session(session, sql, &[]),
        other => panic!("unknown write route {other}"),
    }
}

fn read(db: &EmbeddedDatabase, session: SessionId, route: &str, sql: &str) -> Result<usize, Error> {
    match route {
        "query_with_columns_for_session" => db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows.len()),
        "query_in_session" => db.query_in_session(session, sql, &[]).map(|rows| rows.len()),
        "query_params_for_session" => db.query_params_for_session(session, sql, &[]).map(|rows| rows.len()),
        other => panic!("unknown read route {other}"),
    }
}

/// The refusal must be PostgreSQL's wording for the object.
fn assert_denied<T: std::fmt::Debug>(result: Result<T, Error>, object: &str, context: &str) {
    let error = match result {
        Ok(value) => panic!("{context}: must be refused, but ran and returned {value:?}"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(&format!("permission denied for {object}")),
        "{context}: expected `permission denied for {object}`, got: {error}"
    );
}

fn setup() -> EmbeddedDatabase {
    let db = db();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, total INT)")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (1, 'ann', 10), (2, 'bob', 20)").unwrap();
    db.execute("CREATE TABLE secrets (id INT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO secrets VALUES (1, 'classified')").unwrap();
    db.execute("CREATE ROLE reader LOGIN").unwrap();
    db
}

// ---------------------------------------------------------------------------
// The four data privileges, on every route
// ---------------------------------------------------------------------------

#[test]
fn a_role_without_a_grant_reads_nothing_on_any_route() {
    let db = setup();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_denied(
            read(&db, reader, route, "SELECT * FROM orders"),
            "table orders",
            route,
        );
        assert_denied(
            read(&db, reader, route, "SELECT id FROM orders WHERE id = 1"),
            "table orders",
            route,
        );
        assert_denied(
            read(&db, reader, route, "SELECT count(*) FROM orders"),
            "table orders",
            route,
        );
    }
}

#[test]
fn select_is_granted_and_nothing_else_comes_with_it() {
    let db = setup();
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_eq!(read(&db, reader, route, "SELECT * FROM orders").unwrap(), 2, "{route}");
        assert_eq!(
            read(&db, reader, route, "SELECT id FROM orders WHERE id = 1").unwrap(),
            1,
            "{route}: the primary-key read"
        );
    }
    for route in WRITE_ROUTES {
        assert_denied(
            write(&db, reader, route, "INSERT INTO orders VALUES (3, 'cy', 30)"),
            "table orders",
            route,
        );
        assert_denied(
            write(&db, reader, route, "UPDATE orders SET total = 0 WHERE id = 1"),
            "table orders",
            route,
        );
        assert_denied(
            write(&db, reader, route, "DELETE FROM orders WHERE id = 1"),
            "table orders",
            route,
        );
        assert_denied(write(&db, reader, route, "TRUNCATE orders"), "table orders", route);
    }
    // Nothing was written by the refused statements.
    let rows = db.query("SELECT id, total FROM orders ORDER BY id", &[]).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::Int4(10));
}

#[test]
fn each_write_privilege_admits_its_own_statement_only() {
    for (privilege, allowed) in [
        ("INSERT", "INSERT INTO orders VALUES (9, 'zed', 90)"),
        ("UPDATE", "UPDATE orders SET total = 11"),
        ("DELETE", "DELETE FROM orders"),
        ("TRUNCATE", "TRUNCATE orders"),
    ] {
        let statements = [
            "INSERT INTO orders VALUES (9, 'zed', 90)",
            "UPDATE orders SET total = 11",
            "DELETE FROM orders",
            "TRUNCATE orders",
        ];
        for route in WRITE_ROUTES {
            let db = setup();
            db.execute(&format!("GRANT {privilege} ON orders TO reader")).unwrap();
            let reader = login(&db, "reader");
            for statement in statements {
                let result = write(&db, reader, route, statement);
                if statement == allowed {
                    result.unwrap_or_else(|e| panic!("{route}: {privilege} must admit `{statement}`: {e}"));
                } else {
                    assert_denied(result, "table orders", &format!("{route}: {privilege} vs `{statement}`"));
                }
            }
        }
    }
}

#[test]
fn an_update_with_a_where_clause_also_needs_select() {
    // PostgreSQL: UPDATE … WHERE reads the column the predicate names.
    let db = setup();
    db.execute("GRANT UPDATE ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    for route in WRITE_ROUTES {
        assert_denied(
            write(&db, reader, route, "UPDATE orders SET total = 5 WHERE customer = 'ann'"),
            "table orders",
            route,
        );
        assert_denied(
            write(&db, reader, route, "DELETE FROM orders WHERE id = 1"),
            "table orders",
            route,
        );
    }
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    for route in WRITE_ROUTES {
        write(&db, reader, route, "UPDATE orders SET total = 5 WHERE customer = 'ann'")
            .unwrap_or_else(|e| panic!("{route}: {e}"));
    }
}

#[test]
fn returning_needs_select_on_what_it_returns() {
    let db = setup();
    db.execute("GRANT INSERT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    assert_denied(
        db.execute_returning_for_session(reader, "INSERT INTO orders VALUES (5, 'eve', 50) RETURNING total"),
        "table orders",
        "INSERT … RETURNING without SELECT",
    );
    assert_eq!(
        db.query("SELECT count(*) FROM orders", &[]).unwrap()[0].values[0],
        Value::Int8(2),
        "the refused INSERT … RETURNING must not have inserted"
    );
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    let (count, rows) = db
        .execute_returning_for_session(reader, "INSERT INTO orders VALUES (5, 'eve', 50) RETURNING total")
        .unwrap();
    assert_eq!((count, rows.len()), (1, 1));
}

// ---------------------------------------------------------------------------
// Every table a statement reads is checked
// ---------------------------------------------------------------------------

#[test]
fn a_forbidden_table_is_refused_wherever_the_statement_hides_it() {
    let db = setup();
    db.execute("GRANT SELECT, INSERT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    let hidden = [
        "SELECT * FROM orders WHERE id IN (SELECT id FROM secrets)",
        "SELECT id, (SELECT body FROM secrets WHERE secrets.id = orders.id) FROM orders",
        "SELECT * FROM orders WHERE EXISTS (SELECT 1 FROM secrets)",
        "SELECT o.id FROM orders o JOIN secrets s ON s.id = o.id",
        "SELECT id FROM orders UNION SELECT id FROM secrets",
        "WITH s AS (SELECT * FROM secrets) SELECT o.id FROM orders o, s",
        "SELECT * FROM (SELECT body FROM secrets) AS d",
        "SELECT count(*) FROM secrets",
    ];
    for route in READ_ROUTES {
        for sql in hidden {
            assert_denied(read(&db, reader, route, sql), "table secrets", &format!("{route}: {sql}"));
        }
    }
    for route in WRITE_ROUTES {
        assert_denied(
            write(&db, reader, route, "INSERT INTO orders SELECT id, body, 0 FROM secrets"),
            "table secrets",
            route,
        );
        assert_denied(
            write(
                &db,
                reader,
                route,
                "INSERT INTO orders VALUES (7, (SELECT body FROM secrets WHERE id = 1), 0)",
            ),
            "table secrets",
            route,
        );
    }
    assert_eq!(
        db.query("SELECT count(*) FROM orders", &[]).unwrap()[0].values[0],
        Value::Int8(2)
    );
}

#[test]
fn explain_needs_what_the_statement_needs() {
    let db = setup();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_denied(
            read(&db, reader, route, "EXPLAIN SELECT * FROM secrets"),
            "table secrets",
            route,
        );
    }
}

// ---------------------------------------------------------------------------
// PUBLIC, superusers, the embedded API, anonymous sessions
// ---------------------------------------------------------------------------

#[test]
fn a_grant_to_public_reaches_every_role() {
    let db = setup();
    db.execute("CREATE ROLE other LOGIN").unwrap();
    db.execute("GRANT SELECT ON orders TO PUBLIC").unwrap();
    for role in ["reader", "other"] {
        let session = login(&db, role);
        for route in READ_ROUTES {
            assert_eq!(read(&db, session, route, "SELECT * FROM orders").unwrap(), 2, "{role} {route}");
            assert_denied(read(&db, session, route, "SELECT * FROM secrets"), "table secrets", route);
        }
    }
    db.execute("REVOKE SELECT ON orders FROM PUBLIC").unwrap();
    let session = login(&db, "other");
    assert_denied(
        read(&db, session, READ_ROUTES[0], "SELECT * FROM orders"),
        "table orders",
        "after REVOKE … FROM PUBLIC",
    );
}

#[test]
fn a_superuser_role_and_the_bootstrap_roles_are_not_checked() {
    let db = setup();
    db.execute("CREATE ROLE boss LOGIN SUPERUSER").unwrap();
    for role in ["boss", "postgres", "helios"] {
        let session = login(&db, role);
        for route in READ_ROUTES {
            assert_eq!(read(&db, session, route, "SELECT * FROM secrets").unwrap(), 1, "{role} {route}");
        }
        write(&db, session, WRITE_ROUTES[0], "UPDATE orders SET total = total + 1").unwrap();
    }
    // Losing SUPERUSER takes effect on the open session's next statement.
    let boss = login(&db, "boss");
    assert_eq!(read(&db, boss, READ_ROUTES[0], "SELECT * FROM secrets").unwrap(), 1);
    db.execute("ALTER ROLE boss NOSUPERUSER").unwrap();
    assert_denied(
        read(&db, boss, READ_ROUTES[0], "SELECT * FROM secrets"),
        "table secrets",
        "after NOSUPERUSER",
    );
}

#[test]
fn the_embedded_api_is_unrestricted() {
    let db = setup();
    // No session, no role: the application owns the process.
    assert_eq!(db.query("SELECT * FROM secrets", &[]).unwrap().len(), 1);
    assert_eq!(db.query_params("SELECT * FROM secrets WHERE id = $1", &[Value::Int4(1)]).unwrap().len(), 1);
    db.execute("INSERT INTO secrets VALUES (2, 'more')").unwrap();
    db.execute_params("UPDATE secrets SET body = $1 WHERE id = 2", &[Value::String("x".into())])
        .unwrap();
    db.execute("DROP TABLE secrets").unwrap();
}

#[test]
fn a_network_session_without_a_role_is_refused() {
    let db = setup();
    db.execute("GRANT SELECT ON orders TO PUBLIC").unwrap();
    // What a listener creates before its client has authenticated.
    let anonymous = db.create_network_session("listener").unwrap();
    for route in READ_ROUTES {
        // Even a table PUBLIC may read: nobody is asking.
        let error = read(&db, anonymous, route, "SELECT * FROM orders").unwrap_err().to_string();
        assert!(
            error.contains("permission denied for table orders") && error.contains("no authenticated role"),
            "{route}: {error}"
        );
    }
    for route in WRITE_ROUTES {
        assert_denied(
            write(&db, anonymous, route, "INSERT INTO orders VALUES (8, 'x', 1)"),
            "table orders",
            route,
        );
        let error = write(&db, anonymous, route, "CREATE TABLE sneaky (id INT)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("permission denied"), "{route}: {error}");
    }
    // A statement that needs no privilege still answers.
    assert_eq!(read(&db, anonymous, READ_ROUTES[0], "SELECT 1").unwrap(), 1);
    assert!(db.query("SELECT * FROM sneaky", &[]).is_err(), "nothing was created");
}

#[test]
fn a_login_that_names_no_role_is_the_servers_own_account() {
    // The account given at start, or a name a trust listener accepted: it is
    // not in the catalog, so there is nothing to check it against.
    let db = setup();
    let operator = login(&db, "app_service_account");
    assert_eq!(read(&db, operator, READ_ROUTES[0], "SELECT * FROM secrets").unwrap(), 1);
    // A role created under that name later does not demote the open session,
    // and a new session under the name is the role.
    db.execute("CREATE ROLE app_service_account LOGIN").unwrap();
    let as_role = login(&db, "app_service_account");
    assert_denied(
        read(&db, as_role, READ_ROUTES[0], "SELECT * FROM secrets"),
        "table secrets",
        "a session opened after the role exists",
    );
}

#[test]
fn dropping_a_role_does_not_promote_its_open_session() {
    let db = setup();
    let reader = login(&db, "reader");
    assert_denied(
        read(&db, reader, READ_ROUTES[0], "SELECT * FROM secrets"),
        "table secrets",
        "before the drop",
    );
    db.execute("DROP ROLE reader").unwrap();
    for route in READ_ROUTES {
        assert_denied(
            read(&db, reader, route, "SELECT * FROM secrets"),
            "table secrets",
            &format!("{route}: after DROP ROLE"),
        );
    }
}

// ---------------------------------------------------------------------------
// REVOKE, caches and fast paths
// ---------------------------------------------------------------------------

#[test]
fn revoke_takes_effect_on_the_next_statement_of_an_open_session() {
    let db = setup();
    db.execute("GRANT SELECT, UPDATE ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        // Repeated: the plan cache, the result cache and the primary-key fast
        // path are all warm by the time the grant is taken away.
        for _ in 0..6 {
            assert_eq!(read(&db, reader, route, "SELECT * FROM orders").unwrap(), 2);
            assert_eq!(read(&db, reader, route, "SELECT total FROM orders WHERE id = 1").unwrap(), 1);
        }
    }
    for route in WRITE_ROUTES {
        write(&db, reader, route, "UPDATE orders SET total = 1 WHERE id = 1").unwrap();
    }
    db.execute("REVOKE SELECT, UPDATE ON orders FROM reader").unwrap();
    for route in READ_ROUTES {
        assert_denied(read(&db, reader, route, "SELECT * FROM orders"), "table orders", route);
        assert_denied(
            read(&db, reader, route, "SELECT total FROM orders WHERE id = 1"),
            "table orders",
            route,
        );
    }
    for route in WRITE_ROUTES {
        assert_denied(
            write(&db, reader, route, "UPDATE orders SET total = 1 WHERE id = 1"),
            "table orders",
            route,
        );
    }
}

#[test]
fn a_cached_answer_is_not_served_to_a_role_that_may_not_ask() {
    let db = setup();
    db.execute("CREATE ROLE trusted LOGIN").unwrap();
    db.execute("GRANT SELECT ON secrets TO trusted").unwrap();
    let trusted = login(&db, "trusted");
    let reader = login(&db, "reader");
    let statements = [
        "SELECT * FROM secrets",
        "SELECT body FROM secrets WHERE id = 1",
        "SELECT count(*) FROM secrets",
    ];
    for route in READ_ROUTES {
        for sql in statements {
            // Warm every cache as a role that may read, and as the embedded API.
            for _ in 0..6 {
                read(&db, trusted, route, sql).unwrap();
                db.query(sql, &[]).unwrap();
            }
            assert_denied(read(&db, reader, route, sql), "table secrets", &format!("{route}: {sql}"));
            // And the refusal did not poison the cache for those who may.
            assert_eq!(read(&db, trusted, route, sql).unwrap(), 1, "{route}: {sql}");
        }
    }
}

#[test]
fn a_parameterised_statement_is_checked_each_time_it_runs() {
    let db = setup();
    db.execute("GRANT SELECT, INSERT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    let select = "SELECT total FROM orders WHERE id = $1";
    let insert = "INSERT INTO orders VALUES ($1, 'p', 1)";
    for n in 0..4 {
        assert_eq!(db.query_params_for_session(reader, select, &[Value::Int4(1)]).unwrap().len(), 1);
        db.execute_params_for_session(reader, insert, &[Value::Int4(100 + n)]).unwrap();
    }
    db.execute("REVOKE ALL ON orders FROM reader").unwrap();
    assert_denied(
        db.query_params_for_session(reader, select, &[Value::Int4(1)]),
        "table orders",
        "the cached parameterised SELECT",
    );
    assert_denied(
        db.execute_params_for_session(reader, insert, &[Value::Int4(500)]),
        "table orders",
        "the cached parameterised INSERT",
    );
}

#[test]
fn a_sql_prepared_statement_loses_access_after_revoke() {
    let db = setup();
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    db.execute_for_session(reader, "PREPARE by_id (INT) AS SELECT total FROM orders WHERE id = $1")
        .unwrap();
    assert_eq!(
        db.query_with_columns_for_session(reader, "EXECUTE by_id(1)").unwrap().0.len(),
        1
    );
    db.execute("REVOKE SELECT ON orders FROM reader").unwrap();
    assert_denied(
        db.query_with_columns_for_session(reader, "EXECUTE by_id(1)"),
        "table orders",
        "EXECUTE after REVOKE",
    );
}

#[test]
fn a_transaction_is_checked_statement_by_statement() {
    let db = setup();
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    let reader = login(&db, "reader");
    db.begin_transaction_for_session(reader).unwrap();
    assert_eq!(read(&db, reader, READ_ROUTES[0], "SELECT * FROM orders").unwrap(), 2);
    assert_denied(
        write(&db, reader, WRITE_ROUTES[0], "INSERT INTO orders VALUES (3, 'c', 3)"),
        "table orders",
        "inside a transaction",
    );
    assert_denied(
        read(&db, reader, READ_ROUTES[0], "SELECT * FROM secrets"),
        "table secrets",
        "inside a transaction",
    );
    let _ = db.rollback_transaction_for_session(reader);
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[test]
fn a_view_is_read_with_the_callers_right_on_the_view_and_the_owners_on_its_tables() {
    let db = setup();
    db.execute("CREATE VIEW order_totals AS SELECT id, total FROM orders").unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_denied(
            read(&db, reader, route, "SELECT * FROM order_totals"),
            "view order_totals",
            route,
        );
    }
    db.execute("GRANT SELECT ON order_totals TO reader").unwrap();
    for route in READ_ROUTES {
        // The caller holds nothing on `orders`; the view's owner does.
        for _ in 0..3 {
            assert_eq!(read(&db, reader, route, "SELECT * FROM order_totals").unwrap(), 2, "{route}");
        }
        assert_eq!(
            read(&db, reader, route, "SELECT v.total FROM order_totals v WHERE v.id = 1").unwrap(),
            1,
            "{route}"
        );
        // The view does not open the table behind it.
        assert_denied(read(&db, reader, route, "SELECT * FROM orders"), "table orders", route);
        assert_denied(
            read(
                &db,
                reader,
                route,
                "SELECT * FROM order_totals v JOIN orders o ON o.id = v.id",
            ),
            "table orders",
            route,
        );
    }
}

#[test]
fn a_view_does_not_lend_rights_its_owner_does_not_have() {
    let db = setup();
    db.execute("CREATE ROLE author LOGIN").unwrap();
    db.execute("GRANT CREATE ON SCHEMA public TO author").unwrap();
    db.execute("GRANT SELECT ON orders TO author").unwrap();
    let author = login(&db, "author");
    db.execute_for_session(author, "CREATE VIEW peek AS SELECT body FROM secrets")
        .unwrap();
    db.execute_for_session(author, "CREATE VIEW mine AS SELECT id FROM orders")
        .unwrap();
    db.execute_for_session(author, "GRANT SELECT ON peek TO reader").unwrap();
    db.execute_for_session(author, "GRANT SELECT ON mine TO reader").unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_eq!(read(&db, reader, route, "SELECT * FROM mine").unwrap(), 2, "{route}");
        // `author` owns the view but may not read `secrets`, so nobody reads
        // `secrets` through it: not the reader, not the author.
        assert_denied(read(&db, reader, route, "SELECT * FROM peek"), "table secrets", route);
        assert_denied(read(&db, author, route, "SELECT * FROM peek"), "table secrets", route);
    }
    // Once the owner may read the table, the view works for its grantees.
    db.execute("GRANT SELECT ON secrets TO author").unwrap();
    assert_eq!(read(&db, reader, READ_ROUTES[0], "SELECT * FROM peek").unwrap(), 1);
}

#[test]
fn a_view_over_a_view_checks_each_owner_in_turn() {
    let db = setup();
    db.execute("CREATE VIEW inner_v AS SELECT id, total FROM orders").unwrap();
    db.execute("CREATE VIEW outer_v AS SELECT id FROM inner_v").unwrap();
    db.execute("GRANT SELECT ON outer_v TO reader").unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_eq!(read(&db, reader, route, "SELECT * FROM outer_v").unwrap(), 2, "{route}");
        assert_denied(read(&db, reader, route, "SELECT * FROM inner_v"), "view inner_v", route);
    }
}

// ---------------------------------------------------------------------------
// Durability
// ---------------------------------------------------------------------------

#[test]
fn grants_and_owners_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE orders (id INT PRIMARY KEY, total INT)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 10)").unwrap();
        db.execute("CREATE TABLE secrets (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE ROLE reader LOGIN").unwrap();
        db.execute("CREATE ROLE maker LOGIN").unwrap();
        db.execute("GRANT SELECT ON orders TO reader").unwrap();
        db.execute("GRANT CREATE ON SCHEMA public TO maker").unwrap();
        let maker = login(&db, "maker");
        db.execute_for_session(maker, "CREATE TABLE made (id INT PRIMARY KEY)")
            .unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let reader = login(&db, "reader");
    for route in READ_ROUTES {
        assert_eq!(read(&db, reader, route, "SELECT * FROM orders").unwrap(), 1, "{route}");
        assert_denied(read(&db, reader, route, "SELECT * FROM secrets"), "table secrets", route);
        assert_denied(read(&db, reader, route, "SELECT * FROM made"), "table made", route);
    }
    // The creator still owns what it created.
    let maker = login(&db, "maker");
    db.execute_for_session(maker, "INSERT INTO made VALUES (1)").unwrap();
    db.execute_for_session(maker, "DROP TABLE made").unwrap();
}

#[test]
fn recorded_grants_of_an_earlier_release_take_effect_unchanged() {
    // A database whose roles and grants were recorded while nothing enforced
    // them: after the upgrade the same records decide.
    let db = setup();
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    let (rows, _) = db
        .query_with_columns(
            "SELECT grantee, table_name, privilege_type FROM information_schema.table_privileges",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "the grant is visible where it always was");
    let reader = login(&db, "reader");
    assert_eq!(read(&db, reader, READ_ROUTES[0], "SELECT * FROM orders").unwrap(), 2);
    assert_denied(
        read(&db, reader, READ_ROUTES[0], "SELECT * FROM secrets"),
        "table secrets",
        "a table with no recorded grant",
    );
    // A table with no recorded owner belongs to the bootstrap superuser.
    assert!(db
        .execute_for_session(reader, "DROP TABLE secrets")
        .unwrap_err()
        .to_string()
        .contains("must be owner of table secrets"));
}
