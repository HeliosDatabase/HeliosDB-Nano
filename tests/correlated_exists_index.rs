//! Correlated EXISTS residuals retain outer rows through indexed candidate scans.
//! Exact results on both SQL families; the strict PG35 query/oracle stays unchanged.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];
fn query(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Tuple> {
    match family {
        Family::Text => db.query_with_columns(sql).map(|result| result.0),
        Family::Params => db.query_params(sql, &[]),
    }
    .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"))
}
fn names(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<String> {
    let mut names: Vec<_> = query(db, family, sql)
        .into_iter()
        .map(|row| {
            assert_eq!(row.values.len(), 1);
            match &row.values[0] {
                Value::String(name) => name.clone(),
                value => panic!("expected name, got {value:?}"),
            }
        })
        .collect();
    names.sort();
    names
}
fn seed() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for sql in [
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)",
        "CREATE TABLE orders (order_id INT PRIMARY KEY, customer_id INT)",
        "INSERT INTO customers VALUES (1,'a'),(2,'b'),(3,'c')",
        "INSERT INTO orders VALUES (10,1),(11,1),(12,2),(13,99),(14,NULL)",
    ] {
        db.execute(sql).unwrap();
    }
    db
}
#[test]
fn exists_and_not_exists_use_the_current_point_in_and_range_candidate() {
    for family in FAMILIES {
        let db = seed();
        for (sql, expected) in [
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id)", vec!["a", "b"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id=1", vec!["a"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id=3", vec![]),
            ("SELECT name FROM customers WHERE NOT EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id=1", vec![]),
            ("SELECT name FROM customers WHERE NOT EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id=3", vec!["c"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id IN(1,2,3)", vec!["a", "b"]),
            ("SELECT name FROM customers WHERE NOT EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id IN(1,2,3)", vec!["c"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=3", vec!["a", "b"]),
            ("SELECT name FROM customers WHERE NOT EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=3", vec!["c"]),
            ("SELECT c.name FROM customers c WHERE EXISTS(SELECT 1 FROM orders o WHERE o.customer_id=c.id) AND c.id=2", vec!["b"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id AND order_id<0) AND id IN(1,2,3)", vec![]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE order_id=10) AND id=3", vec!["c"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE order_id<0) AND id=1", vec![]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE ABS(customer_id-customers.id)=0) AND id=1", vec!["a"]),
            ("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE CAST(customer_id AS BIGINT)=CAST(customers.id AS BIGINT)) AND id IN(1,2,3)", vec!["a", "b"]),
            ("SELECT name FROM customers WHERE CAST(EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AS BOOLEAN) AND id=1", vec!["a"]),
            ("SELECT name FROM customers WHERE COALESCE(EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id),FALSE) AND id=3", vec![]),
        ] {
            let expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
            for _ in 0..2 { assert_eq!(names(&db, family, sql), expected, "{family:?}: {sql}"); }
        }
        // The outer bind belongs to this statement; the qualified reference is
        // still supplied independently by each candidate row.
        assert_eq!(db.query_params("SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id=$1", &[Value::Int4(2)]).unwrap()[0].values, vec![Value::String("b".into())]);
    }
}
#[test]
fn pg35_selective_range_ordered_topk_and_counts_keep_correlated_predicates() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("CREATE TABLE orders (order_id INT PRIMARY KEY, customer_id INT)")
            .unwrap();
        let customers = (1..=200)
            .map(|id| format!("({id},'Customer_{id}')"))
            .collect::<Vec<_>>()
            .join(",");
        let orders = (1..=500)
            .map(|id| format!("({id},{})", id % 200 + 1))
            .collect::<Vec<_>>()
            .join(",");
        db.execute(&format!("INSERT INTO customers VALUES {customers}"))
            .unwrap();
        db.execute(&format!("INSERT INTO orders VALUES {orders}")).unwrap();
        db.execute("CREATE INDEX idx_ord_cust ON orders(customer_id)").unwrap();
        let mut expected = (1..=50).map(|id| format!("Customer_{id}")).collect::<Vec<_>>();
        expected.sort();
        let strict =
            "SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=50";
        assert_eq!(names(&db, family, strict), expected);
        assert_eq!(query(&db, family, "SELECT id FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=50 ORDER BY id ASC LIMIT 5").into_iter().map(|row| row.values).collect::<Vec<_>>(), (1..=5).map(|id| vec![Value::Int4(id)]).collect::<Vec<_>>());
        for (sql, count) in [
            ("SELECT COUNT(*) FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=50", 50),
            ("SELECT COUNT(*) FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id AND order_id<0) AND id<=50", 0),
            ("SELECT COUNT(*) FROM customers WHERE NOT EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id<=50", 0),
        ] { assert_eq!(query(&db, family, sql)[0].values, vec![Value::Int8(count)], "{family:?}: {sql}"); }
    }
}
#[test]
fn correlated_candidate_subquery_effect_runs_once_and_errors_propagate() {
    for family in FAMILIES {
        let db = seed();
        db.execute("CREATE TABLE one_order (customer_id INT)").unwrap();
        db.execute("INSERT INTO one_order VALUES (1)").unwrap();
        db.execute("CREATE SEQUENCE correlation_seen START 1").unwrap();
        assert_eq!(names(&db, family, "SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM one_order WHERE customer_id=customers.id AND nextval('correlation_seen')>0) AND id=1"), vec!["a"]);
        assert_eq!(
            query(&db, family, "SELECT currval('correlation_seen')")[0].values,
            vec![Value::Int8(1)]
        );
        let sql = "SELECT name FROM customers WHERE NOT EXISTS(SELECT 1 FROM one_order WHERE customer_id=customers.id AND 1/(customer_id-customer_id)>0) AND id=1";
        let error = match family {
            Family::Text => db.query_with_columns(sql).unwrap_err(),
            Family::Params => db.query_params(sql, &[]).unwrap_err(),
        };
        assert!(
            error.to_string().to_ascii_lowercase().contains("division by zero"),
            "{error}"
        );
        assert_eq!(names(&db, family, "SELECT name FROM customers WHERE id=1"), vec!["a"]);
    }
}

#[test]
fn correlated_exists_keeps_staged_inner_rows_savepoints_and_repeatable_read() {
    use heliosdb_nano::session::IsolationLevel;
    for family in FAMILIES {
        let db = seed();
        let session = db
            .create_session("correlated_staged", IsolationLevel::ReadCommitted)
            .unwrap();
        let select = "SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id IN(1,2,3)";
        let seen = |session| {
            let rows = match family {
                Family::Text => db
                    .query_with_columns_for_session(session, select)
                    .map(|result| result.0),
                Family::Params => db.query_params_for_session(session, select, &[]),
            }
            .unwrap();
            let mut names: Vec<String> = rows
                .into_iter()
                .map(|row| match &row.values[0] {
                    Value::String(name) => name.clone(),
                    value => panic!("expected name: {value:?}"),
                })
                .collect();
            names.sort();
            names
        };
        let execute = |session, sql: &str| {
            match family {
                Family::Text => db.execute_for_session(session, sql),
                Family::Params => db.execute_params_for_session(session, sql, &[]),
            }
            .unwrap()
        };
        db.begin_transaction_for_session(session).unwrap();
        execute(session, "SAVEPOINT before_inner");
        execute(session, "DELETE FROM orders WHERE customer_id=1");
        execute(session, "INSERT INTO orders VALUES (30,3)");
        assert_eq!(
            seen(session),
            vec!["b", "c"],
            "{family:?}: inner staged rows belong to the outer transaction"
        );
        execute(session, "ROLLBACK TO SAVEPOINT before_inner");
        assert_eq!(
            seen(session),
            vec!["a", "b"],
            "{family:?}: inner savepoint state restored"
        );
        db.commit_transaction_for_session(session).unwrap();
        db.destroy_session(session).unwrap();

        let reader = db
            .create_session("correlated_rr", IsolationLevel::RepeatableRead)
            .unwrap();
        db.begin_transaction_for_session(reader).unwrap();
        assert_eq!(seen(reader), vec!["a", "b"]);
        db.execute("DELETE FROM orders WHERE customer_id=1").unwrap();
        db.execute("INSERT INTO orders VALUES (31,3)").unwrap();
        assert_eq!(
            seen(reader),
            vec!["a", "b"],
            "{family:?}: inner relation must retain the reader's snapshot"
        );
        assert_eq!(
            names(&db, family, select),
            vec!["b", "c"],
            "new transaction sees the committed inner change"
        );
        db.commit_transaction_for_session(reader).unwrap();
        db.destroy_session(reader).unwrap();
    }
}
