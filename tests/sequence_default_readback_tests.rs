//! Catalog readback of column DEFAULT expressions (v3.60.1).
//!
//! Column defaults are stored as serialized `LogicalExpr` for evaluation;
//! introspection (pg_dump / ORMs) must read them back as SQL text. Before the
//! fix, `information_schema.columns.column_default` and
//! `pg_get_expr(pg_attrdef.adbin, adrelid)` did not surface the real default
//! for an explicit `DEFAULT nextval('seq')` column (a2h Pagila migration). The
//! default itself always evaluated correctly; only the readback was wrong.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod default_readback {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    /// The sequence store's persistence handle is PROCESS-GLOBAL and last-
    /// writer-wins (see tests/sequence_cache_default.rs for the full note).
    /// Under the threaded harness a sibling test's engine can overwrite the
    /// global and drop, so this file's `DEFAULT nextval(...)` evaluates
    /// against a dead Weak and inserts NULL (observed 2026-07-17 in the full
    /// --no-fail-fast gate: "expected int, got Null"). Serialize until the
    /// handle is per-instance (filed: unify SERIAL/sequence store).
    static SEQ_GLOBAL_HANDLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SEQ_GLOBAL_HANDLE.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn s(v: &Value) -> String {
        match v {
            Value::String(x) => x.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn column_default_renders_back_to_sql() {
        let _serial = serial();
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE SEQUENCE actor_actor_id_seq").unwrap();
        db.execute("CREATE TABLE actor (actor_id integer DEFAULT nextval('actor_actor_id_seq'), name text)")
            .unwrap();
        db.execute("CREATE TABLE t2 (id int DEFAULT 5, x text DEFAULT 'hi', b boolean DEFAULT true)")
            .unwrap();

        // information_schema.columns.column_default reads back as SQL text.
        let rows = db
            .query(
                "SELECT column_name, column_default FROM information_schema.columns WHERE table_name = 'actor'",
                &[],
            )
            .unwrap();
        let actor_default = rows
            .iter()
            .find(|r| s(&r.values[0]) == "actor_id")
            .map(|r| s(&r.values[1]));
        assert_eq!(
            actor_default.as_deref(),
            Some("nextval('actor_actor_id_seq')"),
            "column_default for actor.actor_id should render the nextval expr, got {actor_default:?}"
        );

        // pg_get_expr over pg_attrdef returns the rendered default for an
        // explicit-DEFAULT (non-identity) column — previously absent/NULL.
        let rows = db
            .query("SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef", &[])
            .unwrap();
        let rendered: Vec<String> = rows.iter().map(|r| s(&r.values[0])).collect();
        assert!(
            rendered.iter().any(|d| d.contains("nextval('actor_actor_id_seq')")),
            "pg_get_expr should render the actor_id default; got {rendered:?}"
        );

        // Plain literal defaults render too.
        let rows = db
            .query(
                "SELECT column_name, column_default FROM information_schema.columns WHERE table_name = 't2'",
                &[],
            )
            .unwrap();
        let m: std::collections::HashMap<String, String> =
            rows.iter().map(|r| (s(&r.values[0]), s(&r.values[1]))).collect();
        assert_eq!(m.get("id").map(String::as_str), Some("5"), "DEFAULT 5");
        assert_eq!(m.get("x").map(String::as_str), Some("'hi'"), "DEFAULT 'hi'");
        assert_eq!(m.get("b").map(String::as_str), Some("true"), "DEFAULT true");
    }

    #[test]
    fn explicit_default_still_evaluates() {
        let _serial = serial();
        // The readback fix must not disturb the functional default path.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE SEQUENCE s1").unwrap();
        db.execute("CREATE TABLE a (id integer DEFAULT nextval('s1'), v text)")
            .unwrap();
        db.execute("INSERT INTO a (v) VALUES ('x')").unwrap();
        db.execute("INSERT INTO a (v) VALUES ('y')").unwrap();
        let rows = db.query("SELECT id FROM a ORDER BY id", &[]).unwrap();
        let ids: Vec<i64> = rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int4(n) => *n as i64,
                Value::Int8(n) => *n,
                Value::Int2(n) => *n as i64,
                other => panic!("expected int, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![1, 2], "DEFAULT nextval must auto-increment");
    }
}
