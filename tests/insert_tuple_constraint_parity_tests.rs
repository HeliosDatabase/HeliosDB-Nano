//! PRIMARY KEY / UNIQUE enforcement parity across every `insert_tuple` arm.
//!
//! `StorageEngine` has three insert arms, and which one runs is decided by
//! configuration, not by the caller:
//!
//!   * non-versioned arm — `insert_tuple` when `storage.time_travel_enabled == false`
//!   * versioned arm     — `insert_tuple` when it is `true`, and
//!                         `insert_tuple_versioned{,_with_schema}` unconditionally
//!   * SQL fast path     — `insert_tuple_fast` (every `INSERT` statement)
//!
//! The non-versioned arm never checked PK/UNIQUE. `time_travel_enabled = false`
//! is what the shipped `fast` and `fast_ingest` profiles set (`src/config.rs`),
//! so on those profiles every entry point that funnels into plain `insert_tuple`
//! — `POST /rest/v1/<table>`, dump RESTORE, the protocol adapters, materialized
//! view refresh — accepted duplicate primary keys: silently, durably, no error.
//! The measured before/after, same tuple inserted twice:
//!
//! | arm                          | before        | after         |
//! |------------------------------|---------------|---------------|
//! | `insert_tuple`, TT = true    | rejected, 1 row | rejected, 1 row |
//! | `insert_tuple`, TT = **false** | **accepted, 2 rows** | rejected, 1 row |
//! | SQL `INSERT`, TT = either    | rejected, 1 row | rejected, 1 row |
//!
//! Every case below therefore runs the SAME logical insert through EVERY arm
//! under BOTH settings of `time_travel_enabled` and asserts they agree — and
//! that the agreed-on answer is the correct one. Half of these tests were
//! already green before the fix; they are here so the rule cannot silently
//! regress on one arm again.
//!
//! Two assertion conventions, both borrowed from `tests/rls_write_parity_tests.rs`:
//!   * predicates are pushed into the SQL (`… WHERE payload = 'first'`) instead of
//!     comparing `Value`s, so nothing depends on which integer width the engine
//!     happened to store for a given path;
//!   * a rejected write is always followed by a ground-truth read, because
//!     "returned an error" and "wrote nothing" are independent claims.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};

/// Both storage profiles' settings. `fast`/`fast_ingest` select `false`;
/// `safe`/`balanced`/`agent` select `true`.
const TIME_TRAVEL: [bool; 2] = [true, false];

fn db_with_time_travel(time_travel: bool) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = time_travel;
    EmbeddedDatabase::with_config(config).unwrap()
}

/// The three ways a row reaches the storage engine's insert arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// `StorageEngine::insert_tuple` — dispatches on `time_travel_enabled`, so
    /// this is the ONLY path that reaches the non-versioned arm. REST, RESTORE,
    /// the protocol adapters and MV refresh all land here.
    Plain,
    /// `StorageEngine::insert_tuple_versioned` — the versioned arm under BOTH
    /// settings (with `time_travel_enabled = false` it just skips the version
    /// write), which is how the versioned arm gets covered on a `fast` profile.
    Versioned,
    /// A SQL `INSERT` statement — `insert_tuple_fast`. Never had the bug; pinned
    /// here so a future "unification" cannot quietly drop its check.
    Sql,
}

impl Path {
    const ALL: [Self; 3] = [Self::Plain, Self::Versioned, Self::Sql];

    const fn label(self) -> &'static str {
        match self {
            Self::Plain => "insert_tuple",
            Self::Versioned => "insert_tuple_versioned",
            Self::Sql => "SQL INSERT",
        }
    }
}

/// Insert `(id, text)` into a two-column table through `path`.
/// `text = None` inserts SQL NULL.
fn insert(db: &EmbeddedDatabase, path: Path, table: &str, id: i32, text: Option<&str>) -> Result<(), String> {
    match path {
        Path::Plain | Path::Versioned => {
            let value = text.map_or(Value::Null, |s| Value::String(s.to_string()));
            let tuple = Tuple::new(vec![Value::Int4(id), value]);
            let res = if path == Path::Plain {
                db.storage.insert_tuple(table, tuple)
            } else {
                db.storage.insert_tuple_versioned(table, tuple)
            };
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        Path::Sql => {
            let literal = text.map_or_else(|| "NULL".to_string(), |s| format!("'{s}'"));
            db.execute(&format!("INSERT INTO {table} VALUES ({id}, {literal})"))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}

/// Rows matching a query. The predicate lives in the SQL, so the count is the
/// assertion — no `Value` comparison, no dependency on stored integer width.
fn count(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

/// `CREATE TABLE <name> (id INTEGER PRIMARY KEY, payload TEXT)`.
fn create_pk_table(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY, payload TEXT)"))
        .unwrap();
}

/// `CREATE TABLE <name> (id INTEGER PRIMARY KEY, payload TEXT UNIQUE)` — the
/// UNIQUE column is nullable, which is what makes the NULL test below meaningful.
fn create_unique_table(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!(
        "CREATE TABLE {name} (id INTEGER PRIMARY KEY, payload TEXT UNIQUE)"
    ))
    .unwrap();
}

/// Assert a write was refused as a duplicate. Matches on `uplicate` because the
/// storage arms surface the ART's `Duplicate key: …` while a SQL-path rejection
/// may be reworded PostgreSQL-style (`duplicate key value violates …`).
fn assert_rejected(result: &Result<(), String>, context: &str) {
    let err = result
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("{context}: duplicate was ACCEPTED — it must be rejected"));
    assert!(
        err.contains("uplicate"),
        "{context}: rejected, but not as a duplicate: {err}"
    );
}

// ---------------------------------------------------------------------------
// Duplicate PRIMARY KEY — the measured defect
// ---------------------------------------------------------------------------

/// THE regression. `insert_tuple` must refuse a duplicate PK whether or not
/// time-travel is on: enforcement is a property of the table, not of the
/// storage profile. Before the fix this failed on the `false` iteration with
/// two rows carrying `id = 1`.
#[test]
fn plain_insert_tuple_rejects_duplicate_pk_under_both_time_travel_settings() {
    for time_travel in TIME_TRAVEL {
        let db = db_with_time_travel(time_travel);
        create_pk_table(&db, "t");

        insert(&db, Path::Plain, "t", 1, Some("first"))
            .unwrap_or_else(|e| panic!("time_travel={time_travel}: the FIRST insert must succeed: {e}"));
        let dup = insert(&db, Path::Plain, "t", 1, Some("second"));

        assert_rejected(&dup, &format!("insert_tuple, time_travel={time_travel}"));
        assert_eq!(
            count(&db, "SELECT id FROM t"),
            1,
            "time_travel={time_travel}: the rejected duplicate must not be durable"
        );
        assert_eq!(
            count(&db, "SELECT id FROM t WHERE payload = 'first'"),
            1,
            "time_travel={time_travel}: the surviving row must be the FIRST one"
        );
        assert_eq!(
            count(&db, "SELECT id FROM t WHERE payload = 'second'"),
            0,
            "time_travel={time_travel}: the duplicate's payload must be nowhere in the table"
        );
    }
}

/// The versioned arm reached directly — the one arm that always checked. It is
/// pinned under `time_travel_enabled = false` too, because that is the
/// configuration in which it skips the version write and is easiest to
/// "simplify" into the arm that had the bug.
#[test]
fn versioned_insert_tuple_rejects_duplicate_pk_under_both_time_travel_settings() {
    for time_travel in TIME_TRAVEL {
        let db = db_with_time_travel(time_travel);
        create_pk_table(&db, "t");

        insert(&db, Path::Versioned, "t", 1, Some("first"))
            .unwrap_or_else(|e| panic!("time_travel={time_travel}: the FIRST insert must succeed: {e}"));
        let dup = insert(&db, Path::Versioned, "t", 1, Some("second"));

        assert_rejected(&dup, &format!("insert_tuple_versioned, time_travel={time_travel}"));
        assert_eq!(
            count(&db, "SELECT id FROM t"),
            1,
            "time_travel={time_travel}: versioned arm must not persist the duplicate"
        );
        assert_eq!(count(&db, "SELECT id FROM t WHERE payload = 'second'"), 0);
    }
}

/// Regression pin: the SQL/wire surface was never affected and must stay that
/// way. Wire clients (psql, psycopg, sqlx, the MySQL listener) all land here.
#[test]
fn sql_insert_still_rejects_duplicate_pk_under_both_time_travel_settings() {
    for time_travel in TIME_TRAVEL {
        let db = db_with_time_travel(time_travel);
        create_pk_table(&db, "t");

        insert(&db, Path::Sql, "t", 1, Some("first"))
            .unwrap_or_else(|e| panic!("time_travel={time_travel}: the FIRST insert must succeed: {e}"));
        let dup = insert(&db, Path::Sql, "t", 1, Some("second"));

        assert_rejected(&dup, &format!("SQL INSERT, time_travel={time_travel}"));
        assert_eq!(
            count(&db, "SELECT id FROM t"),
            1,
            "time_travel={time_travel}: SQL path must keep exactly the first row"
        );
        assert_eq!(count(&db, "SELECT id FROM t WHERE payload = 'first'"), 1);
    }
}

/// The rule must not depend on WHICH arm wrote the first row either: a row
/// inserted through any arm must block a duplicate arriving through any other.
/// This is the whole matrix — 3 seeding paths x 3 duplicating paths x 2 settings
/// — and it is what "one rule, one implementation" actually means.
#[test]
fn duplicate_pk_is_rejected_for_every_seed_path_and_every_duplicate_path() {
    for time_travel in TIME_TRAVEL {
        let db = db_with_time_travel(time_travel);

        for (i, seed) in Path::ALL.into_iter().enumerate() {
            for (j, dup_path) in Path::ALL.into_iter().enumerate() {
                let table = format!("m{i}{j}");
                create_pk_table(&db, &table);
                let context = format!(
                    "time_travel={time_travel}, seeded via {}, duplicated via {}",
                    seed.label(),
                    dup_path.label()
                );

                insert(&db, seed, &table, 1, Some("first"))
                    .unwrap_or_else(|e| panic!("{context}: the FIRST insert must succeed: {e}"));
                let dup = insert(&db, dup_path, &table, 1, Some("second"));

                assert_rejected(&dup, &context);
                assert_eq!(
                    count(&db, &format!("SELECT id FROM {table}")),
                    1,
                    "{context}: exactly the first row must remain"
                );
                assert_eq!(
                    count(&db, &format!("SELECT id FROM {table} WHERE payload = 'first'")),
                    1,
                    "{context}: the surviving row must be the first one"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Duplicate UNIQUE (non-PK)
// ---------------------------------------------------------------------------

/// A UNIQUE column that is not the primary key. The duplicating row carries a
/// DISTINCT primary key, so only the UNIQUE index can reject it — this fails if
/// an arm checks the PK index alone.
#[test]
fn duplicate_unique_non_pk_is_rejected_on_every_path_and_setting() {
    for time_travel in TIME_TRAVEL {
        for path in Path::ALL {
            let db = db_with_time_travel(time_travel);
            create_unique_table(&db, "u");
            let context = format!("{}, time_travel={time_travel}", path.label());

            insert(&db, path, "u", 1, Some("taken"))
                .unwrap_or_else(|e| panic!("{context}: the FIRST insert must succeed: {e}"));
            let dup = insert(&db, path, "u", 2, Some("taken"));

            assert_rejected(&dup, &context);
            assert_eq!(
                count(&db, "SELECT id FROM u"),
                1,
                "{context}: the row that duplicates a UNIQUE value must not be durable"
            );
            assert_eq!(
                count(&db, "SELECT id FROM u WHERE payload = 'taken'"),
                1,
                "{context}: 'taken' must appear exactly once"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Over-rejection guard — a check that rejects too much is a worse bug
// ---------------------------------------------------------------------------

/// Distinct rows must still insert, on every arm and under both settings. A
/// pre-insert gate that rejects legitimate writes would be a worse defect than
/// the one it fixes, and it would be invisible to the tests above.
#[test]
fn distinct_rows_still_insert_on_every_path_and_setting() {
    for time_travel in TIME_TRAVEL {
        for path in Path::ALL {
            let db = db_with_time_travel(time_travel);
            create_unique_table(&db, "t");
            let context = format!("{}, time_travel={time_travel}", path.label());

            for id in 1..=5 {
                let payload = format!("row{id}");
                insert(&db, path, "t", id, Some(payload.as_str()))
                    .unwrap_or_else(|e| panic!("{context}: distinct row {id} must insert: {e}"));
            }

            assert_eq!(
                count(&db, "SELECT id FROM t"),
                5,
                "{context}: all five distinct rows must be present"
            );
            for id in 1..=5 {
                assert_eq!(
                    count(
                        &db,
                        &format!("SELECT id FROM t WHERE id = {id} AND payload = 'row{id}'")
                    ),
                    1,
                    "{context}: row {id} must be readable with its own payload"
                );
            }
        }
    }
}

/// Distinct rows inserted through DIFFERENT arms into the same table must all
/// survive — the mixed-arm shape a RESTORE (plain `insert_tuple`) followed by
/// application traffic (SQL) produces.
#[test]
fn rows_from_different_paths_coexist_under_both_settings() {
    for time_travel in TIME_TRAVEL {
        let db = db_with_time_travel(time_travel);
        create_pk_table(&db, "t");

        insert(&db, Path::Plain, "t", 1, Some("plain")).unwrap();
        insert(&db, Path::Versioned, "t", 2, Some("versioned")).unwrap();
        insert(&db, Path::Sql, "t", 3, Some("sql")).unwrap();

        assert_eq!(
            count(&db, "SELECT id FROM t"),
            3,
            "time_travel={time_travel}: one row per arm must be present"
        );
        assert_eq!(count(&db, "SELECT id FROM t WHERE id = 1 AND payload = 'plain'"), 1);
        assert_eq!(count(&db, "SELECT id FROM t WHERE id = 2 AND payload = 'versioned'"), 1);
        assert_eq!(count(&db, "SELECT id FROM t WHERE id = 3 AND payload = 'sql'"), 1);
    }
}

/// SQL semantics: two NULLs in a nullable UNIQUE column are NOT duplicates of
/// each other (PostgreSQL's default `NULLS DISTINCT`). Verified against the
/// engine before being asserted: `ArtIndexManager::check_unique_constraints{,_tuple}`
/// skip any index whose key contains a NULL, so this pins existing behaviour —
/// the gate added to the non-versioned arm must not narrow it.
#[test]
fn null_unique_values_are_not_duplicates_on_every_path_and_setting() {
    for time_travel in TIME_TRAVEL {
        for path in Path::ALL {
            let db = db_with_time_travel(time_travel);
            create_unique_table(&db, "u");
            let context = format!("{}, time_travel={time_travel}", path.label());

            insert(&db, path, "u", 1, None)
                .unwrap_or_else(|e| panic!("{context}: first NULL payload must insert: {e}"));
            insert(&db, path, "u", 2, None).unwrap_or_else(|e| {
                panic!("{context}: a second NULL in a UNIQUE column is NOT a duplicate (SQL NULLS DISTINCT): {e}")
            });

            assert_eq!(
                count(&db, "SELECT id FROM u"),
                2,
                "{context}: both NULL-payload rows must be durable"
            );
            assert_eq!(
                count(&db, "SELECT id FROM u WHERE payload IS NULL"),
                2,
                "{context}: both rows must read back with a NULL payload"
            );
            // …and a non-NULL duplicate in that same column is still rejected,
            // so the NULL exemption is not a hole in the constraint.
            insert(&db, path, "u", 3, Some("x")).unwrap_or_else(|e| panic!("{context}: 'x' must insert: {e}"));
            assert_rejected(&insert(&db, path, "u", 4, Some("x")), &context);
            assert_eq!(
                count(&db, "SELECT id FROM u"),
                3,
                "{context}: 4 rows means the dup landed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Heap / index agreement after a rejection
// ---------------------------------------------------------------------------

/// After a rejected duplicate the heap and the ART must describe the same
/// table. This is the assertion the old behaviour failed *twice over*: the
/// non-versioned arm wrote the row, then `art_index_manager.on_insert` refused
/// the duplicate index key and the failure was swallowed at `tracing::debug!`,
/// so a full scan saw two rows while an indexed lookup saw one — with nothing
/// in the log at any shipped level.
#[test]
fn rejected_duplicate_leaves_scan_and_index_agreeing() {
    for time_travel in TIME_TRAVEL {
        for path in Path::ALL {
            let db = db_with_time_travel(time_travel);
            create_pk_table(&db, "t");
            let context = format!("{}, time_travel={time_travel}", path.label());

            insert(&db, path, "t", 1, Some("first"))
                .unwrap_or_else(|e| panic!("{context}: the FIRST insert must succeed: {e}"));
            assert_rejected(&insert(&db, path, "t", 1, Some("second")), &context);

            let full_scan = count(&db, "SELECT id FROM t");
            let indexed_lookup = count(&db, "SELECT id FROM t WHERE id = 1");
            assert_eq!(
                full_scan, 1,
                "{context}: full scan sees {full_scan} row(s); exactly the first row must remain"
            );
            assert_eq!(
                indexed_lookup, full_scan,
                "{context}: DIVERGENCE — indexed lookup sees {indexed_lookup} row(s), full scan sees {full_scan}"
            );

            // Index-level ground truth: the PK key is registered exactly once,
            // and it is the surviving row's key.
            let schema = db.storage.catalog().get_table_schema("t").unwrap();
            let pk_column = schema
                .columns
                .iter()
                .find(|c| c.primary_key)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| panic!("{context}: table 't' must have a primary key column"));
            assert!(
                db.storage
                    .art_indexes()
                    .unique_key_exists("t", &[pk_column], &[Value::Int4(1)]),
                "{context}: the surviving row's PK must be present in the ART index"
            );
        }
    }
}
