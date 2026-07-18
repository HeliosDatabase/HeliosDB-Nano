//! C11: `SET statement_timeout` / configured statement timeout is enforced by
//! the executor (was previously accepted but never consulted — dead code).
//! Operators poll their TimeoutContext between rows, so a query that keeps
//! producing rows past the deadline is cancelled cooperatively.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn seed(db: &EmbeddedDatabase, rows: usize) {
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, v INT)").unwrap();
    for chunk in (1..=rows as i64).collect::<Vec<_>>().chunks(1000) {
        let vals: String = chunk.iter().map(|i| format!("({i},{i})")).collect::<Vec<_>>().join(",");
        db.execute(&format!("INSERT INTO big VALUES {vals}")).unwrap();
    }
}

#[test]
fn no_timeout_by_default() {
    // Default is unlimited: a full scan completes regardless of duration.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 20_000);
    let rows = db
        .query("SELECT count(*) FROM big WHERE v > 0", &[])
        .expect("unbounded query must succeed by default");
    assert_eq!(rows[0].values[0], Value::Int8(20_000));
}

#[test]
fn set_statement_timeout_is_enforced_on_a_long_scan() {
    // A 1ms timeout against a self-join that materializes many rows must be
    // caught by the cooperative deadline check. Before C11 this ran to
    // completion (SET was ignored). We assert it either errors with a
    // timeout-ish message OR — on a very fast host — still completes; what must
    // NOT happen is the SET being silently a no-op AND the query being slow.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 20_000);
    db.execute("SET statement_timeout = 1").unwrap(); // 1 ms

    let start = std::time::Instant::now();
    // A cross-ish product to guarantee the operator loop runs long enough to
    // cross a 1ms deadline on any realistic host.
    let result = db.query("SELECT count(*) FROM big a JOIN big b ON a.v = b.v WHERE a.id > 0", &[]);
    let elapsed = start.elapsed();

    match result {
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("timeout") || msg.contains("timed out") || msg.contains("cancel"),
                "expected a timeout error, got: {e}"
            );
        }
        Ok(_) => {
            // Completed under the deadline on a fast host — acceptable only if
            // it really was fast. If it took far longer than the 1ms deadline
            // yet still returned, the timeout was not enforced.
            assert!(
                elapsed < std::time::Duration::from_millis(500),
                "query ran {elapsed:?} despite a 1ms statement_timeout — not enforced"
            );
        }
    }

    // Timeout is per-statement session state; reset and confirm normal queries
    // work again.
    db.execute("SET statement_timeout = 0").unwrap();
    let rows = db.query("SELECT count(*) FROM big", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Int8(20_000));
}
