//! GH#34 — `CREATE INDEX` on an EXPRESSION is rejected with
//! "Column name expected in CREATE INDEX" (`src/sql/planner.rs:1209`).
//!
//! Copy to `tests/gh_issue_34.rs`.
//!
//! # What is broken
//!
//! `Statement::CreateIndex` is planned in ONE arm
//! (`src/sql/planner.rs:1173`). At `src/sql/planner.rs:1205-1210` the leading
//! key is matched against `Expr::Identifier` and EVERYTHING else is refused:
//!
//! ```ignore
//! let column = match &first_col.expr {
//!     Expr::Identifier(ident) => Self::normalize_ident(ident),
//!     _ => return Err(Error::query_execution("Column name expected in CREATE INDEX")),
//! };
//! ```
//!
//! Both executor families reach that one arm — `db.execute()` (text /
//! simple-query) and `db.execute_params()` (the PostgreSQL EXTENDED protocol
//! and the REST layer) — so every test below runs on BOTH, and a fix in one
//! says nothing about the other.
//!
//! mem0 2.0.20's `create_col()` issues
//! `CREATE INDEX IF NOT EXISTS <c>_text_lemmatized_idx ON <c>
//!  USING gin(to_tsvector('simple', payload->>'text_lemmatized'))`
//! unconditionally, so collection creation fails outright.
//!
//! # How to read a failure
//!
//! The tests are tiered on purpose, so a partial fix reports WHICH half is
//! missing:
//!
//!   * `ddl_*`      — TIER A: the grammar is accepted, the definition is
//!                    persisted / droppable / introspectable. A catalog-only
//!                    ("accept and record") implementation passes these.
//!   * `indexed_*`  — TIER B: the btree/ART expression index is REALLY BUILT —
//!                    the evaluated expression is the ART key, maintained on
//!                    INSERT / UPDATE / DELETE and restored at reopen. An
//!                    accept-and-ignore implementation FAILS these, which is
//!                    the point: a planner that later claims to use a
//!                    silently-unindexed expression is worse than the error
//!                    this issue reports.
//!   * `rejects_*`  — FAIL-CLOSED: a non-immutable or unresolvable index
//!                    expression must still be refused, and refused for the
//!                    RIGHT reason (not by the blanket "Column name expected").
//!   * `control_*`  — POSITIVE CONTROL: passes before AND after the fix. If a
//!                    control fails, the harness (not the fix) is broken.
//!
//! # Adversarial-review corrections (2026-09-08)
//!
//!  1. RETRACTED: the first triage pass claimed a SECOND, independent breakage
//!     in `Parser::preprocess_create_index_using` (`src/sql/parser.rs:2377`) —
//!     that `remaining[paren_content_start..].find(')')` (`parser.rs:2426`)
//!     "emits unbalanced SQL" for a nested call. It does not: the rewrite
//!     deletes exactly one `(` and one `)` and re-inserts one of each in the
//!     same relative order, so it is paren-preserving by construction. See the
//!     doc block on `ddl_gin_expression_with_nested_calls_is_accepted`. That
//!     statement still fails, but at `planner.rs:1209` like every other one.
//!  2. CORRECTED: the `CREATE UNIQUE INDEX` refusal at `src/sql/planner.rs:1296`
//!     is not an independent fourth breakage — it is DEAD CODE for a LEADING
//!     expression, because the blanket check at `planner.rs:1207-1210` runs
//!     first. See `rejects_unique_index_over_a_leading_expression_or_really_enforces_it`.
//!  3. ADDED: `indexed_expression_lookup_is_consulted_and_usable_with_bound_params`
//!     — nothing in the original file proved the index is ever CONSULTED, nor
//!     exercised a real bound `$1` through the lookup path.
//!  4. HARDENED: every TIER B test now also asserts the encoding-independent
//!     `index_entry_count`, so a fix that encodes keys differently cannot be
//!     failed spuriously; and the `rejects_*` tests now pin WHY, not just
//!     "not the blanket message".
//!
//! `gin_index_duplicate_name_and_unknown_column_are_rejected` covers a RELATED pre-existing hole
//! found while reading the fix site: the `IndexFamily::DdlOnly` branch of
//! `src/sql/executor/ddl.rs:312-345` never checks `index_exists` and never
//! consults the table schema, so a duplicate gin index name silently succeeds
//! (PostgreSQL: 42P07) and `USING gin (no_such_column)` is accepted. It is
//! marked in its own test so it can be triaged separately.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{sql::SystemViewRegistry, storage::ArtIndexManager};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// `false` = the text family (`db.execute()` → `execute_in_transaction_inner`:
/// psql simple query, MySQL wire, embedded).
/// `true`  = the params family (`db.execute_params()` →
/// `execute_plan_with_params_inner`: the PG EXTENDED protocol every real
/// driver uses, plus REST/BaaS).
const FAMILIES: [bool; 2] = [false, true];

fn family(params_family: bool) -> &'static str {
    if params_family {
        "params"
    } else {
        "text"
    }
}

fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn must_run(db: &EmbeddedDatabase, sql: &str, params_family: bool) {
    if let Err(e) = run(db, sql, params_family) {
        panic!("[{}] `{}` must succeed, got: {}", family(params_family), sql, e);
    }
}

fn must_fail(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> String {
    match run(db, sql, params_family) {
        Ok(_) => panic!(
            "[{}] `{}` must be REJECTED, but it succeeded",
            family(params_family),
            sql
        ),
        Err(e) => e.to_string(),
    }
}

fn ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("expected an integer first column, got {other:?}"),
        })
        .collect()
}

fn select_ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    ids(&rows)
}

/// `EXPLAIN <sql>` flattened to one string, in the shape
/// `tests/index_range_scan_tests.rs:20 rows_exact` uses.
fn explain_text(db: &EmbeddedDatabase, sql: &str) -> String {
    let rows = db
        .query(&format!("EXPLAIN {sql}"), &[])
        .unwrap_or_else(|e| panic!("`EXPLAIN {sql}` failed: {e}"));
    rows.iter()
        .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// ENCODING-INDEPENDENT "was it built?" probe: `Some(n)` iff `index_name` is a
/// LIVE ART registration, and `n` is its total `(key, row_id)` entry count
/// (`src/storage/art_manager.rs:1130 index_entry_count`).
///
/// ADVERSARIAL-REVIEW ADDITION. `index_row_count` below probes a key built with
/// `ArtIndexManager::encode_key(&[value])`. That is the encoding every existing
/// ART path uses (`insert_row_indexes`, `art_manager.rs:1610`; `on_insert`,
/// `art_manager.rs:2444`; `backfill_manual_index`, `art_manager.rs:714`) and the
/// one `tests/a3_secondary_index_lookup.rs:19` already asserts on — but it is
/// still an implementation assumption. If an implementer encodes an expression
/// key differently, `index_row_count` would report 0 and this file would fail a
/// CORRECT fix. `index_entry_count` cannot: it counts the tree, whatever the
/// key bytes are. Every TIER B test therefore asserts BOTH.
fn index_entry_count(db: &EmbeddedDatabase, index_name: &str) -> Option<u64> {
    db.storage.art_indexes().index_entry_count(index_name)
}

/// How many rows the ART index `index_name` holds under the key produced by
/// `value`. This is the assertion that separates "the index was BUILT" from
/// "the statement was accepted and nothing happened" — a catalog-only
/// implementation returns 0 here forever.
fn index_row_count(db: &EmbeddedDatabase, index_name: &str, value: Value) -> usize {
    let key = ArtIndexManager::encode_key(&[value]);
    db.storage.art_indexes().index_get_all(index_name, &key).len()
}

/// `pg_indexes.indexdef` for one index name, through the exported system-view
/// registry (`src/sql/system_views.rs:1128 execute_pg_indexes`, the copy
/// `heliosdb_nano::sql::SystemViewRegistry` resolves to). NOTE: that copy walks
/// the LIVE ART registry, so it only ever sees an index that is really
/// registered — which is precisely why the assertion on it lives in a TIER B
/// test. A second copy renders the same view for `SELECT * FROM pg_indexes`
/// (`src/sql/phase3/system_views.rs:4159`); a fix must update BOTH.
fn pg_indexes_def(db: &EmbeddedDatabase, index_name: &str) -> Option<String> {
    let rows = SystemViewRegistry::new().execute("pg_indexes", &db.storage).ok()?;
    rows.iter().find_map(|row| {
        let name = match row.values.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => return None,
        };
        if !name.eq_ignore_ascii_case(index_name) {
            return None;
        }
        match row.values.get(4) {
            Some(Value::String(def)) => Some(def.clone()),
            _ => Some(String::new()),
        }
    })
}

/// The mem0-shaped fixture: `id INT PRIMARY KEY, payload JSONB`, four rows.
/// (mem0's real table also has a `vector(768)` column; it is irrelevant to the
/// index grammar and is exercised separately in
/// `ddl_mem0_create_col_sequence_is_accepted`.)
fn seed(db: &EmbeddedDatabase, table: &str, params_family: bool) {
    // CREATE TABLE always runs on the text family (house style: only the
    // statement UNDER TEST varies by family, so an unrelated DDL difference
    // cannot be mistaken for this issue).
    db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, payload JSONB)"))
        .unwrap_or_else(|e| panic!("fixture CREATE TABLE {table} failed: {e}"));
    for (id, json) in [
        (1, r#"{"user_id":"u1","text_lemmatized":"alpha"}"#),
        (2, r#"{"user_id":"u2","text_lemmatized":"beta"}"#),
        (3, r#"{"user_id":"u1","text_lemmatized":"gamma"}"#),
        (4, r#"{"other_key":"z"}"#),
    ] {
        must_run(
            db,
            &format!("INSERT INTO {table} VALUES ({id}, '{json}')"),
            params_family,
        );
    }
}

// ---------------------------------------------------------------------------
// POSITIVE CONTROLS — must pass before AND after the fix
// ---------------------------------------------------------------------------

/// Everything the issue lists as already working. If this test fails, the fix
/// is not what broke — the harness or an unrelated regression is.
#[test]
fn control_column_indexes_gin_and_to_tsvector_all_work() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_ctl_{}", family(params_family));
        seed(&db, &table, params_family);

        // Bare column, btree: OK today.
        must_run(
            &db,
            &format!("CREATE INDEX gh34_ctl_a_{} ON {table} (id)", family(params_family)),
            params_family,
        );
        // Bare column, gin: OK today (DDL-only by design — docs/compatibility/fts.md).
        must_run(
            &db,
            &format!(
                "CREATE INDEX gh34_ctl_b_{} ON {table} USING gin (payload)",
                family(params_family)
            ),
            params_family,
        );

        // to_tsvector on its own: OK today.
        let rows = db
            .query("SELECT to_tsvector('simple','hello world') IS NOT NULL", &[])
            .expect("to_tsvector must evaluate");
        assert_eq!(rows.len(), 1, "[{}] one row expected", family(params_family));
        assert_eq!(
            rows[0].values.first(),
            Some(&Value::Boolean(true)),
            "[{}] to_tsvector('simple', …) must be non-NULL",
            family(params_family)
        );

        // The unindexed expression predicate is already correct — the fix must
        // keep exactly these answers.
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' = 'u1' ORDER BY id")
            ),
            vec![1, 3],
            "[{}] expression predicate without any index",
            family(params_family)
        );
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'other_key' = 'z' ORDER BY id")
            ),
            vec![4],
            "[{}] non-matching expression predicate without any index",
            family(params_family)
        );

        // ADVERSARIAL-REVIEW ADDITION — the EXPLAIN harness itself works TODAY.
        // Without this control, a failure of
        // `indexed_expression_lookup_is_consulted_and_usable_with_bound_params`
        // could not be distinguished from "EXPLAIN doesn't annotate anything in
        // this build". Proven shape: `tests/uuid_index_probe_explain.rs:59`.
        let text = explain_text(&db, &format!("SELECT payload FROM {table} WHERE id = 1"));
        assert!(
            text.contains("Index Point Lookup using"),
            "[{}] EXPLAIN must already annotate a plain COLUMN equality probe \
             (src/sql/executor/explain.rs:136); got:\n{text}",
            family(params_family)
        );
    }
}

// ---------------------------------------------------------------------------
// TIER A — DDL acceptance
// ---------------------------------------------------------------------------

/// `CREATE INDEX t_c ON mem0 ((payload->>'user_id'))` — the plain btree half of
/// the issue. Fails today at `src/sql/planner.rs:1209`.
#[test]
fn ddl_parenthesised_btree_expression_index_is_accepted() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_btree_{}", family(params_family));
        seed(&db, &table, params_family);

        let idx = format!("gh34_btree_uid_{}", family(params_family));
        must_run(
            &db,
            &format!("CREATE INDEX {idx} ON {table} ((payload->>'user_id'))"),
            params_family,
        );

        // Accepting it must not have changed any answer.
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' = 'u1' ORDER BY id")
            ),
            vec![1, 3],
            "[{}] results after CREATE INDEX on the expression",
            family(params_family)
        );

        // It is a real, named, droppable object.
        must_run(&db, &format!("DROP INDEX {idx}"), params_family);
        let err = must_fail(&db, &format!("DROP INDEX {idx}"), params_family);
        assert!(
            err.to_lowercase().contains("does not exist"),
            "[{}] second DROP INDEX must report an absent index, got: {err}",
            family(params_family)
        );
    }
}

/// `CREATE INDEX t_d ON mem0 USING gin(to_tsvector('simple', payload->>'x'))`
/// — the GIN half, i.e. the exact statement mem0's `create_col()` emits.
#[test]
fn ddl_gin_expression_index_is_accepted() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_gin_{}", family(params_family));
        seed(&db, &table, params_family);

        let idx = format!("gh34_gin_lemma_{}", family(params_family));
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS {idx} ON {table} \
             USING gin(to_tsvector('simple', payload->>'text_lemmatized'))"
        );
        must_run(&db, &sql, params_family);

        // mem0 re-runs create_col() on every start: IF NOT EXISTS must stay
        // silent on the second run.
        must_run(&db, &sql, params_family);

        // The @@ predicate still scans (documented gin behaviour) and must be
        // correct.
        assert_eq!(
            select_ids(
                &db,
                &format!(
                    "SELECT id FROM {table} \
                     WHERE to_tsvector('simple', payload->>'text_lemmatized') @@ to_tsquery('alpha')"
                )
            ),
            vec![1],
            "[{}] gin-indexed expression predicate must still answer correctly",
            family(params_family)
        );

        must_run(&db, &format!("DROP INDEX {idx}"), params_family);
    }
}

/// A NESTED call inside `USING <am>(...)`.
///
/// ADVERSARIAL-REVIEW CORRECTION (2026-09-08). The first triage pass claimed
/// this shape dies in the PARSER, because
/// `Parser::preprocess_create_index_using` (`src/sql/parser.rs:2377`) locates
/// the key list's closing paren with `remaining[paren_content_start..].find(')')`
/// (`src/sql/parser.rs:2426`) — the FIRST `)`, not the matching one — and so
/// "emits unbalanced SQL". That is WRONG, and the fix plan must not act on it.
///
/// The rewrite is paren-preserving BY CONSTRUCTION: it deletes exactly one `(`
/// (at `paren_start`) and exactly one `)` (at `paren_end`) and re-inserts
/// exactly one of each at the same relative positions
/// (`format!("{} ({}) {};", before_using, column_spec, after_paren)`,
/// `src/sql/parser.rs:2437-2446`). Simulated against the real algorithm, every
/// shape below comes out balanced and semantically identical to the input:
///
/// ```text
/// IN : … USING gin(to_tsvector('simple', coalesce(p->>'a','') || ' ' || coalesce(p->>'b','')))
/// OUT: … (to_tsvector('simple', coalesce(p->>'a','') || ' ' || coalesce(p->>'b','')));   balance = 0
/// IN : … USING gin(f(a)) WITH (fillfactor=70)
/// OUT: … (f(a) ) WITH (fillfactor=70);                                                   balance = 0
/// ```
///
/// So this statement reaches the planner and dies at `src/sql/planner.rs:1209`
/// exactly like the single-call one — same site, same message. The test is kept
/// because a fix MUST handle nesting, but an implementer must not go rewriting
/// `preprocess_create_index_using` believing it is broken here.
///
/// (The one genuine, narrow defect in that rewriter is quote-unawareness: a `)`
/// inside a string literal — `payload->>'a)b'` — has a space injected into the
/// literal by the `") "` re-insertion. Cosmetic-but-real, out of scope for #34,
/// and NOT a balance problem. It is not asserted here.)
#[test]
fn ddl_gin_expression_with_nested_calls_is_accepted() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_nested_{}", family(params_family));
        seed(&db, &table, params_family);

        let idx = format!("gh34_nested_idx_{}", family(params_family));
        must_run(
            &db,
            &format!(
                "CREATE INDEX {idx} ON {table} USING gin(to_tsvector('simple', \
                 coalesce(payload->>'text_lemmatized','') || ' ' || coalesce(payload->>'user_id','')))"
            ),
            params_family,
        );
        must_run(&db, &format!("DROP INDEX {idx}"), params_family);
    }
}

/// The faithful mem0 `create_col()` shape, in order, on one connection.
#[test]
fn ddl_mem0_create_col_sequence_is_accepted() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_mem0_{}", family(params_family));

        // Control: the table shape itself is unrelated to #34 and must build.
        // (mem0's real column is literally named `vector`; renamed here so a
        // reserved-word question cannot be confused with this issue.)
        db.execute(&format!(
            "CREATE TABLE {table} (id UUID PRIMARY KEY, embedding VECTOR(3), payload JSONB)"
        ))
        .unwrap_or_else(|e| panic!("fixture mem0-shaped CREATE TABLE failed (unrelated to #34): {e}"));
        must_run(
            &db,
            &format!(
                "CREATE INDEX IF NOT EXISTS {table}_hnsw_idx ON {table} \
                 USING hnsw (embedding vector_cosine_ops)"
            ),
            params_family,
        );
        // The statement that blocks collection creation today.
        must_run(
            &db,
            &format!(
                "CREATE INDEX IF NOT EXISTS {table}_text_lemmatized_idx ON {table} \
                 USING gin(to_tsvector('simple', payload->>'text_lemmatized'))"
            ),
            params_family,
        );
    }
}

/// Persisted, droppable and introspectable after a reopen — TIER A half of
/// "survival across reopen". No index CONTENT is asserted here; that is
/// `indexed_expression_index_contents_survive_reopen`.
#[test]
fn ddl_expression_index_definition_survives_reopen() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).expect("open");
        seed(&db, "gh34_reopen", false);
        must_run(
            &db,
            "CREATE INDEX gh34_reopen_uid ON gh34_reopen ((payload->>'user_id'))",
            false,
        );
        must_run(
            &db,
            "CREATE INDEX gh34_reopen_gin ON gh34_reopen USING gin(to_tsvector('simple', payload->>'text_lemmatized'))",
            false,
        );
    }

    let db = EmbeddedDatabase::new(temp.path()).expect("reopen");

    // Answers stay correct across the restart.
    assert_eq!(
        select_ids(
            &db,
            "SELECT id FROM gh34_reopen WHERE payload->>'user_id' = 'u1' ORDER BY id"
        ),
        vec![1, 3],
        "expression predicate after reopen"
    );

    // Both survive as droppable catalog objects: the `meta:index:<name>` record
    // is what `DROP INDEX` dispatches on (`src/sql/executor/ddl.rs:772`), so a
    // successful drop after a restart proves the definition was persisted.
    must_run(&db, "DROP INDEX gh34_reopen_uid", false);
    must_run(&db, "DROP INDEX gh34_reopen_gin", false);
}

/// `IF NOT EXISTS` is silent on a repeat; a bare repeat is an error (42P07).
#[test]
fn ddl_expression_index_duplicate_name_semantics() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_dup_{}", family(params_family));
        seed(&db, &table, params_family);

        let ine = format!("gh34_dup_ine_{}", family(params_family));
        let sql = format!("CREATE INDEX IF NOT EXISTS {ine} ON {table} ((payload->>'user_id'))");
        must_run(&db, &sql, params_family);
        must_run(&db, &sql, params_family);

        let bare = format!("gh34_dup_bare_{}", family(params_family));
        let sql = format!("CREATE INDEX {bare} ON {table} ((payload->>'user_id'))");
        must_run(&db, &sql, params_family);
        let err = must_fail(&db, &sql, params_family);
        assert!(
            err.to_lowercase().contains("already exists"),
            "[{}] a duplicate index name must report 'already exists', got: {err}",
            family(params_family)
        );
    }
}

// ---------------------------------------------------------------------------
// TIER B — the index is really built and really maintained
// ---------------------------------------------------------------------------

/// The evaluated expression is the ART key, and it tracks INSERT / UPDATE /
/// DELETE. An accept-and-ignore implementation fails on the FIRST assertion
/// (0 entries), which is the intended signal.
#[test]
fn indexed_expression_key_is_built_and_maintained() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_maint_{}", family(params_family));
        let idx = format!("gh34_maint_uid_{}", family(params_family));
        seed(&db, &table, params_family);

        must_run(
            &db,
            &format!("CREATE INDEX {idx} ON {table} ((payload->>'user_id'))"),
            params_family,
        );

        // (a) ENCODING-INDEPENDENT: the index is a LIVE ART registration and it
        // was backfilled. A catalog-only "accept and record" implementation
        // gives `None` here; a built-but-not-backfilled one gives `Some(0)`.
        // The seed holds 4 rows, one of which (id=4) has no `user_id` key, so
        // its expression is NULL. PostgreSQL btree DOES index NULLs and
        // `key_is_null_distinct` (`src/storage/art_manager.rs:1465`) only skips
        // them for PRIMARY KEY / UNIQUE, so the natural count is 4 — but a fix
        // that legitimately declines to index NULL expressions would give 3.
        // Both are defensible, so the bound is `>= 3` and the precise key
        // probes below carry the precision.
        let built = index_entry_count(&db, &idx);
        assert!(
            built.is_some(),
            "[{}] CREATE INDEX on an expression must register a LIVE ART index (got None — \
             a catalog-only implementation)",
            family(params_family)
        );
        let built = built.unwrap_or(0);
        assert!(
            built >= 3,
            "[{}] CREATE INDEX must BACKFILL the evaluated expression for the 3 non-NULL \
             pre-existing rows; index holds {built} entries",
            family(params_family)
        );

        // (b) Backfill of the rows that existed BEFORE the index, by key.
        assert_eq!(
            index_row_count(&db, &idx, Value::String("u1".into())),
            2,
            "[{}] CREATE INDEX must backfill the evaluated expression for existing rows",
            family(params_family)
        );

        // INSERT maintains it.
        must_run(
            &db,
            &format!(r#"INSERT INTO {table} VALUES (5, '{{"user_id":"u1"}}')"#),
            params_family,
        );
        assert_eq!(
            index_row_count(&db, &idx, Value::String("u1".into())),
            3,
            "[{}] INSERT must add the evaluated expression key",
            family(params_family)
        );
        assert_eq!(
            index_entry_count(&db, &idx),
            Some(built + 1),
            "[{}] INSERT must add exactly one entry to the expression index (encoding-independent)",
            family(params_family)
        );

        // UPDATE moves the key.
        must_run(
            &db,
            &format!(r#"UPDATE {table} SET payload = '{{"user_id":"u9"}}' WHERE id = 1"#),
            params_family,
        );
        assert_eq!(
            index_row_count(&db, &idx, Value::String("u1".into())),
            2,
            "[{}] UPDATE must remove the OLD evaluated key",
            family(params_family)
        );
        assert_eq!(
            index_row_count(&db, &idx, Value::String("u9".into())),
            1,
            "[{}] UPDATE must insert the NEW evaluated key",
            family(params_family)
        );

        // UPDATE must not change the total entry count.
        assert_eq!(
            index_entry_count(&db, &idx),
            Some(built + 1),
            "[{}] UPDATE must MOVE a key, not add or drop one (encoding-independent)",
            family(params_family)
        );

        // DELETE removes it.
        must_run(&db, &format!("DELETE FROM {table} WHERE id = 5"), params_family);
        assert_eq!(
            index_row_count(&db, &idx, Value::String("u1".into())),
            1,
            "[{}] DELETE must remove the evaluated key",
            family(params_family)
        );
        assert_eq!(
            index_entry_count(&db, &idx),
            Some(built),
            "[{}] DELETE must remove exactly one entry (encoding-independent)",
            family(params_family)
        );

        // …and every answer is still right, for the indexed expression and for
        // a DIFFERENT expression over the same column (which the index must not
        // be used for).
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' = 'u1' ORDER BY id")
            ),
            vec![3],
            "[{}] indexed expression predicate",
            family(params_family)
        );
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' = 'u9' ORDER BY id")
            ),
            vec![1],
            "[{}] indexed expression predicate, moved key",
            family(params_family)
        );
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'other_key' = 'z' ORDER BY id")
            ),
            vec![4],
            "[{}] a NON-matching expression must not be answered from this index",
            family(params_family)
        );
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' IS NULL ORDER BY id")
            ),
            vec![4],
            "[{}] rows whose expression evaluates to NULL must still be found",
            family(params_family)
        );
        assert_eq!(
            select_ids(
                &db,
                &format!("SELECT id FROM {table} WHERE payload->>'user_id' <> 'u1' ORDER BY id")
            ),
            vec![1, 2],
            "[{}] inequality over the indexed expression",
            family(params_family)
        );
    }
}

/// Reopen: `Catalog::rebuild_all_indexes` must re-register the expression index
/// AND repopulate it, exactly as it does for a column index.
#[test]
fn indexed_expression_index_contents_survive_reopen() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).expect("open");
        seed(&db, "gh34_rebuild", false);
        must_run(
            &db,
            "CREATE INDEX gh34_rebuild_uid ON gh34_rebuild ((payload->>'user_id'))",
            false,
        );
        assert_eq!(
            index_row_count(&db, "gh34_rebuild_uid", Value::String("u1".into())),
            2,
            "expression index must be populated before the close"
        );
        assert!(
            index_entry_count(&db, "gh34_rebuild_uid").unwrap_or(0) >= 3,
            "expression index must be populated before the close (encoding-independent)"
        );
    }

    let db = EmbeddedDatabase::new(temp.path()).expect("reopen");
    assert!(
        index_entry_count(&db, "gh34_rebuild_uid").is_some(),
        "`Catalog::rebuild_all_indexes` (src/storage/catalog.rs:1574) must RE-REGISTER the \
         expression index at open — a persisted definition with no live registration means \
         every later probe silently full-scans"
    );
    assert_eq!(
        index_row_count(&db, "gh34_rebuild_uid", Value::String("u1".into())),
        2,
        "expression index must be rebuilt from the persisted definition at open"
    );

    // And it keeps being maintained in the new process.
    must_run(&db, r#"INSERT INTO gh34_rebuild VALUES (6, '{"user_id":"u1"}')"#, false);
    assert_eq!(
        index_row_count(&db, "gh34_rebuild_uid", Value::String("u1".into())),
        3,
        "a rebuilt expression index must receive post-reopen inserts"
    );
    assert_eq!(
        select_ids(
            &db,
            "SELECT id FROM gh34_rebuild WHERE payload->>'user_id' = 'u1' ORDER BY id"
        ),
        vec![1, 3, 6],
        "answers after reopen"
    );
}

/// Introspection must show the EXPRESSION, never a fabricated column name — a
/// `\d` / `pg_indexes` row that names a column the table does not have is
/// worse than no row at all.
#[test]
fn indexed_expression_index_is_reported_by_pg_indexes() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    seed(&db, "gh34_introspect", false);
    must_run(
        &db,
        "CREATE INDEX gh34_introspect_uid ON gh34_introspect ((payload->>'user_id'))",
        false,
    );

    let def = pg_indexes_def(&db, "gh34_introspect_uid").expect("the btree expression index must appear in pg_indexes");
    assert!(
        def.contains("user_id"),
        "pg_indexes.indexdef must render the indexed EXPRESSION, got: {def}"
    );

    // A plain column index must keep its byte-identical rendering.
    must_run(&db, "CREATE INDEX gh34_introspect_id ON gh34_introspect (id)", false);
    let def = pg_indexes_def(&db, "gh34_introspect_id").expect("column index in pg_indexes");
    assert!(
        def.contains("(id)"),
        "a plain column index must still render as `(id)`, got: {def}"
    );
}

// ---------------------------------------------------------------------------
// FAIL-CLOSED
// ---------------------------------------------------------------------------

/// A volatile expression cannot be an index key: the stored key would be right
/// only at the instant it was written, and every later probe would miss rows
/// that exist. PostgreSQL: "functions in index expression must be marked
/// IMMUTABLE". This must stay an ERROR — but not the blanket
/// "Column name expected in CREATE INDEX", which is what makes it pass today
/// for the wrong reason.
#[test]
fn rejects_volatile_index_expression() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_vol_{}", family(params_family));
        seed(&db, &table, params_family);

        for (expr, fname) in [("(now())", "now"), ("(random())", "random")] {
            let sql = format!("CREATE INDEX gh34_vol_idx ON {table} ({expr})");
            let err = must_fail(&db, &sql, params_family);
            assert!(
                !err.contains("Column name expected"),
                "[{}] `{sql}` must be refused as non-immutable, not by the blanket \
                 column-name check; got: {err}",
                family(params_family)
            );
            // ADVERSARIAL-REVIEW HARDENING: `!contains(blanket)` alone would be
            // satisfied by ANY other error post-fix, including an accidental
            // one. Require the diagnostic to actually be about immutability, or
            // to at least name the offending function. The fix plan pins
            // PostgreSQL's wording ("functions in index expression must be
            // marked IMMUTABLE"), so `immutable` is the primary hook.
            let lower = err.to_lowercase();
            assert!(
                lower.contains("immutable") || lower.contains(fname),
                "[{}] `{sql}` must be refused with a diagnostic that says WHY (immutability) \
                 or names `{fname}`; got: {err}",
                family(params_family)
            );
        }
    }
}

/// An index expression over a column that does not exist must be refused, and
/// must name the column. Today it is refused by the blanket check, so the user
/// is told the wrong thing.
#[test]
fn rejects_index_expression_over_unknown_column() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_unk_{}", family(params_family));
        seed(&db, &table, params_family);

        let err = must_fail(
            &db,
            &format!("CREATE INDEX gh34_unk_idx ON {table} ((no_such_col->>'x'))"),
            params_family,
        );
        assert!(
            err.contains("no_such_col"),
            "[{}] the error must name the unresolvable column, got: {err}",
            family(params_family)
        );
    }
}

/// An index expression that references NO column of the table indexes one
/// constant key for every row — useless, and a trap. PostgreSQL refuses it
/// ("index expression must reference a column"); so must we.
#[test]
fn rejects_constant_index_expression() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_const_{}", family(params_family));
        seed(&db, &table, params_family);

        let err = must_fail(
            &db,
            &format!("CREATE INDEX gh34_const_idx ON {table} ((1 + 1))"),
            params_family,
        );
        assert!(
            !err.contains("Column name expected"),
            "[{}] a constant index expression must be refused as a constant, not by the \
             blanket column-name check; got: {err}",
            family(params_family)
        );
        // ADVERSARIAL-REVIEW HARDENING: pin the diagnostic to the actual reason
        // so this cannot pass post-fix on an unrelated error. PostgreSQL says
        // "index expression cannot return a set" / "…must reference a column";
        // the fix plan pins "index expression must reference a column".
        assert!(
            err.to_lowercase().contains("expression"),
            "[{}] the refusal must be ABOUT the index expression; got: {err}",
            family(params_family)
        );
    }
}

// ---------------------------------------------------------------------------
// RELATED PRE-EXISTING HOLE (triage separately)
// ---------------------------------------------------------------------------

/// `src/sql/executor/ddl.rs:312-345` — the `IndexFamily::DdlOnly` (gin/gist)
/// branch persists the definition WITHOUT checking `index_exists` and WITHOUT
/// checking that the column exists. So a duplicate gin index name silently
/// succeeds where PostgreSQL raises 42P07, and `USING gin (typo)` is accepted.
/// Independent of #34, but the same branch the fix has to touch.
#[test]
fn gin_index_duplicate_name_and_unknown_column_are_rejected() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_ginhole_{}", family(params_family));
        seed(&db, &table, params_family);

        let idx = format!("gh34_ginhole_idx_{}", family(params_family));
        let sql = format!("CREATE INDEX {idx} ON {table} USING gin (payload)");
        must_run(&db, &sql, params_family);
        let err = must_fail(&db, &sql, params_family);
        assert!(
            err.to_lowercase().contains("already exists"),
            "[{}] a duplicate gin index name must report 'already exists', got: {err}",
            family(params_family)
        );

        let err = must_fail(
            &db,
            &format!(
                "CREATE INDEX gh34_ginhole_bad_{} ON {table} USING gin (no_such_col)",
                family(params_family)
            ),
            params_family,
        );
        assert!(
            err.contains("no_such_col"),
            "[{}] a gin index on an unknown column must be refused by name, got: {err}",
            family(params_family)
        );
    }
}

// ---------------------------------------------------------------------------
// ADVERSARIAL-REVIEW ADDITIONS
// ---------------------------------------------------------------------------

/// TIER B, the half the original triage left untested: an expression index that
/// is BUILT but never CONSULTED is invisible to every assertion above except
/// `index_entry_count`. This pins fix-plan step 11
/// (`src/sql/executor/scan.rs:1040 equality_lookup_from_sides`, which today
/// bails at the `let LogicalExpr::Column { .. } = column_expr else` on line
/// 1048) AND the bound-parameter path the PG EXTENDED protocol actually uses:
/// `db.query_params(… = $1 …)` reaches the SAME lookup through
/// `lookup_bound_value`'s `LogicalExpr::Parameter` arm (`scan.rs:1070`).
///
/// Every other test in this file passes `&[]` to `execute_params`, which
/// exercises the params PLANNER but never a bound value in the lookup path.
/// This one does.
#[test]
fn indexed_expression_lookup_is_consulted_and_usable_with_bound_params() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    seed(&db, "gh34_probe", false);
    // A TWIN with identical contents and NO expression index: the answers must
    // match exactly, which is what proves the index did not change semantics.
    seed(&db, "gh34_probe_twin", false);

    must_run(
        &db,
        "CREATE INDEX gh34_probe_uid ON gh34_probe ((payload->>'user_id'))",
        false,
    );

    let text = explain_text(&db, "SELECT id FROM gh34_probe WHERE payload->>'user_id' = 'u1'");
    assert!(
        text.contains("Index Point Lookup using gh34_probe_uid"),
        "the expression index must actually be CONSULTED for the expression it indexes \
         (src/sql/executor/scan.rs:1040); got:\n{text}"
    );

    let twin = explain_text(&db, "SELECT id FROM gh34_probe_twin WHERE payload->>'user_id' = 'u1'");
    assert!(
        !twin.contains("Index Point Lookup"),
        "the UNINDEXED twin must not claim an index probe — the displayed plan must be the \
         executed plan; got:\n{twin}"
    );

    // A DIFFERENT expression over the same column must NOT be answered from
    // this index (canonical-text match, not "mentions the column"). Wrong rows
    // are the failure mode this guards.
    let other = explain_text(&db, "SELECT id FROM gh34_probe WHERE payload->>'other_key' = 'z'");
    assert!(
        !other.contains("Index Point Lookup using gh34_probe_uid"),
        "a DIFFERENT expression must not be probed against this index; got:\n{other}"
    );

    // Bound parameter — the extended-protocol shape (psycopg / node-pg / sqlx).
    let rows = db
        .query_params(
            "SELECT id FROM gh34_probe WHERE payload->>'user_id' = $1 ORDER BY id",
            &[Value::String("u1".into())],
        )
        .expect("bound-parameter lookup over an expression index");
    assert_eq!(ids(&rows), vec![1, 3], "indexed, bound parameter");

    let rows = db
        .query_params(
            "SELECT id FROM gh34_probe_twin WHERE payload->>'user_id' = $1 ORDER BY id",
            &[Value::String("u1".into())],
        )
        .expect("bound-parameter lookup on the unindexed twin");
    assert_eq!(ids(&rows), vec![1, 3], "unindexed twin must give the SAME answer");

    // A value no row has must return NOTHING, not the whole table and not an
    // error — the classic index-probe failure mode.
    let rows = db
        .query_params(
            "SELECT id FROM gh34_probe WHERE payload->>'user_id' = $1 ORDER BY id",
            &[Value::String("nobody".into())],
        )
        .expect("miss on the expression index");
    assert!(
        ids(&rows).is_empty(),
        "a miss must return zero rows, got {:?}",
        ids(&rows)
    );
}

/// `CREATE UNIQUE INDEX` over a LEADING expression.
///
/// PostgreSQL accepts this and ENFORCES it.
///
/// PLANNER ARM ORDERING — an adversarial-review correction to the original
/// triage, which listed the unique-specific refusal as an independent "FOURTH"
/// breakage. It is not independent, and for this statement it is DEAD CODE:
/// the blanket check at `src/sql/planner.rs:1207-1210` runs FIRST and inspects
/// only the LEADING key, so a leading expression dies there with
/// "Column name expected in CREATE INDEX". The unique-specific message at
/// `src/sql/planner.rs:1292-1301` is reachable ONLY when the leading key is an
/// identifier and a LATER key is an expression
/// (`CREATE UNIQUE INDEX u ON t (id, (payload->>'k'))`). An implementer who
/// "fixes planner.rs:1296" without touching 1209 changes nothing for this test.
/// (That trailing-key shape is deliberately NOT asserted as a control: a
/// complete fix may legitimately start ACCEPTING it, and a control must pass
/// both before and after.)
///
/// The fix has two honest outcomes and this test accepts EITHER, but with
/// teeth — a UNIQUE index is a CONSTRAINT, so "accepted" must mean "enforced":
///
///   * ACCEPTED → a second row with the same evaluated expression MUST be
///     rejected. Accepting the DDL and enforcing nothing is the exact failure
///     this repo already shipped once for `CREATE UNIQUE INDEX` on a column
///     (`src/sql/executor/ddl.rs:88-100`) and must not ship again.
///   * REFUSED  → with a diagnostic that says it is about UNIQUE, never the
///     blanket "Column name expected in CREATE INDEX".
#[test]
fn rejects_unique_index_over_a_leading_expression_or_really_enforces_it() {
    for params_family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let table = format!("gh34_uexpr_{}", family(params_family));
        seed(&db, &table, params_family);

        let idx = format!("gh34_uexpr_idx_{}", family(params_family));
        let sql = format!("CREATE UNIQUE INDEX {idx} ON {table} ((payload->>'other_key'))");

        match run(&db, &sql, params_family) {
            Ok(_) => {
                // Rows 1-3 have no `other_key` (expression NULL, distinct under
                // UNIQUE); row 4 has 'z'. A second 'z' must be refused.
                let err = must_fail(
                    &db,
                    &format!(r#"INSERT INTO {table} VALUES (7, '{{"other_key":"z"}}')"#),
                    params_family,
                );
                assert!(
                    err.to_lowercase().contains("unique")
                        || err.to_lowercase().contains("duplicate")
                        || err.to_lowercase().contains("23505"),
                    "[{}] an ACCEPTED unique expression index must actually ENFORCE; got: {err}",
                    family(params_family)
                );
            }
            Err(e) => {
                let err = e.to_string();
                assert!(
                    !err.contains("Column name expected"),
                    "[{}] `{sql}` must not be refused by the blanket column-name check; got: {err}",
                    family(params_family)
                );
                assert!(
                    err.to_lowercase().contains("unique"),
                    "[{}] if a unique expression index is refused, say it is about UNIQUE; got: {err}",
                    family(params_family)
                );
            }
        }
    }
}
