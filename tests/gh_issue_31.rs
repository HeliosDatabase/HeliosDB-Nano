//! GH #31 — rows with a VARCHAR PRIMARY KEY become unreadable across restarts:
//! `COUNT(*)` still sees them, `SELECT *` and every key predicate does not.
//!
//! Install as `tests/gh_issue_31.rs`.
//!
//! # The reported fingerprint, and what in this engine produces it
//!
//! The reporter's six-row `_migrations` table (VARCHAR(200) PRIMARY KEY, written
//! only by parameterised INSERTs, never updated or deleted) answered, in one
//! session, after ~6h and two container restarts:
//!
//! ```text
//! SELECT count(*)                          -> 6
//! SELECT *                                 -> 4 rows
//! SELECT count(*) WHERE "name" = '0001_init' -> 0
//! SELECT count(*) WHERE "name" < '0004'      -> 0
//! ```
//!
//! Those four answers come from THREE different sources of truth:
//!
//! * `COUNT(*)` with no predicate is answered from the length of the PRIMARY KEY
//!   ART index — `StorageEngine::count_table_rows` (src/storage/engine.rs:7556, PK-ART branch at :7584)
//!   returns `art_index_manager.pk_index_len(table)` (src/storage/art_manager.rs:1186
//!   = `tree.len()`), NOT from the rows.
//! * `SELECT *` walks the `data:{table}:{row_id}` keyspace
//!   (`scan_table_with_schema_opt`, src/storage/engine.rs:7396) and never drops a
//!   row silently (a decode error propagates).
//! * `WHERE pk = …` / `WHERE pk < …` probe the ART, fetch the rows the probe
//!   names by row id, and then RE-APPLY the predicate to each fetched row
//!   (`try_index_point_lookup_for_scan` -> `filter_tuples_with_evaluator`,
//!   src/sql/executor/scan.rs:347 -> :430; the fast twin
//!   `EmbeddedDatabase::fast_select_rows` -> `fast_row_matches_probed_pk`,
//!   src/lib.rs:13088 / 13075).
//!
//! So the fingerprint says exactly one thing: **the PK ART holds a key whose
//! `data:` row is gone or now holds a different row.** The engine's own mechanism
//! for producing that is a REUSED internal row id: `insert_tuple_fast`
//! (src/storage/engine.rs:11262) allocates row ids from a volatile in-memory
//! counter and only re-persists the durable `counter:{table}` key every 64 rows
//! (src/storage/engine.rs:11398, `if row_id % 64 == 0`). A reopen that seeds the
//! counter from a stale durable value hands the next INSERT an already-used row
//! id; `put()` overwrites the older row in place, and the older row's PK entry
//! stays in the ART forever. Count over-reports, projections lose the row, and
//! the key predicates find a key whose row no longer carries it. The row lost is
//! the OLDEST — row id 1 first. That is the report, exactly.
//!
//! # What this file pins
//!
//! * `agree_*` — the invariant, on the reported schema shape, with text keys that
//!   share prefixes / are prefixes of each other / sort either side of the probe,
//!   across TWO close-and-reopen cycles, on BOTH executor families
//!   (`execute`/`query` = text; `execute_params`/`query_params` = the extended
//!   protocol node-pg uses). These lock in the counter-flush ordering fix that
//!   `EmbeddedDatabase::drop` (src/lib.rs:849-870) and the scan-fallback reseed
//!   (`Catalog::rebuild_all_indexes`, src/storage/catalog.rs:1706) already make.
//!   Expected to PASS on the current tree.
//!
//! * `checkpoint_then_crash_*` — the window those two fixes do NOT cover:
//!   `StorageEngine::persist_index_snapshots()` (src/storage/engine.rs:9099) is a
//!   PUBLIC checkpoint API that writes the snapshot validity markers WITHOUT
//!   flushing the row counters. `Drop` gets the ordering right (counters first);
//!   this entry point has no ordering at all. A crash after such a checkpoint
//!   reopens into the state `EmbeddedDatabase::drop`'s own comment calls
//!   unrecoverable — valid snapshot + stale counter — because
//!   `reseed_row_counter_from_max_row_id` is reachable only from the scan
//!   fallback, which a valid snapshot skips. Expected to FAIL on the current tree.
//!
//! * `flush_failure_*` — the same state, reached the way production reaches it
//!   without any test-only API: `EmbeddedDatabase::drop` WARNS on a failed
//!   `flush_all_row_counters()` and then writes the snapshot anyway
//!   (src/lib.rs:849-856 vs 863-870). The `set_row_counter_flush_on_close(false)`
//!   knob models precisely "the counter flush did not happen, the checkpoint
//!   did". Expected to FAIL on the current tree.
//!
//! * `reindex_must_repair_a_diverged_pk_index` and
//!   `a_diverged_pk_index_must_not_survive_a_clean_restart` — the aftermath,
//!   which is what the reporter actually lived with: once the ART and the rows
//!   disagree, NOTHING detects or repairs it. `REINDEX` is an accepted NO-OP
//!   (`try_handle_reindex_statement`, src/lib.rs:2559 — "Nano's index storage has
//!   no user-visible rebuild need to satisfy"), and the clean-close checkpoint
//!   exports the phantom entry verbatim (`export_table_snapshot`,
//!   src/storage/art_manager.rs:382) for the next open to bulk-load straight back
//!   (`load_index_entries`, :414) without ever looking at a row. Both INJECT the
//!   divergence through the public ART API rather than relying on the row-id
//!   reuse above, so they keep testing detection/repair after the reuse route is
//!   closed. Expected to FAIL on the current tree.
//!
//! Tests that FAIL on the current tree are marked ***OPEN*** in their doc comment.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Env var set by the parent to route a re-exec of this test binary into its
/// "crash child" role; its value is the data dir the child must populate.
/// Same technique as `tests/crash_recovery_e2e_test.rs` — and for the same
/// reason: only a process that never runs `Drop` leaves a stale durable row
/// counter behind, and only a dead process releases the RocksDB directory lock
/// so the parent can reopen the same path.
const CRASH_CHILD_ENV: &str = "HELIOS_GH31_CRASH_CHILD_DB_PATH";

fn open(dir: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::new(dir).expect("open disk-backed database")
}

/// Re-exec THIS test binary filtered to `test_name` so its crash-child branch
/// runs, then die via `std::process::exit(0)` (no Rust destructors).
fn crash_via_child(test_name: &str, db_path: &Path) {
    let status = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CRASH_CHILD_ENV, db_path)
        .status()
        .expect("spawn crash-child");
    assert!(
        status.success(),
        "crash-child for '{test_name}' did not exit cleanly: {status:?}"
    );
}

/// The reported DDL shape: a VARCHAR PRIMARY KEY plus a NOT NULL timestamp,
/// with an extra INT so the "predicate on a non-key column" arm of the report
/// can be asserted without depending on timestamp comparison semantics.
const DDL: &str = r#"CREATE TABLE IF NOT EXISTS "_migrations" (
    "name" VARCHAR(200) PRIMARY KEY,
    "appliedAt" TIMESTAMP(3) NOT NULL,
    "ord" INT NOT NULL
)"#;

/// Text primary keys chosen for every ART hazard the report implicates:
///   * `0001` is a STRICT PREFIX of `0001_init`, which is a strict prefix of
///     `0001_init_extra` (single-column ART keys are raw, unterminated bytes —
///     `encode_key_from_values`, src/storage/art_manager.rs:1317 — so a key that
///     ends exactly where another key continues must still be findable);
///   * long shared prefixes (`0001_…` x3, `0004…` x2) drive prefix compression
///     and node splits (`MAX_PREFIX_LEN` truncation, src/storage/art_index.rs);
///   * values sort on both sides of the `'0004'` probe used below;
///   * ASCII only, so Rust's byte ordering and SQL's ordering agree and the
///     `<` expectations below are computed, not hand-written.
const NAMES: [&str; 8] = [
    "0001",
    "0001_init",
    "0001_init_extra",
    "0002_add_users",
    "0003_billing",
    "0004",
    "0004_a",
    "0010_z",
];

/// The probe `WHERE "name" < '0004'` must return exactly these. Computed from
/// Rust's byte ordering rather than hand-written, so the expectation cannot
/// drift from the key set above.
fn below_probe() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for name in NAMES {
        if name < "0004" {
            v.push(name.to_string());
        }
    }
    v.sort();
    v
}

fn seed(db: &EmbeddedDatabase, params_family: bool) {
    db.execute(DDL).expect("create _migrations");
    for (i, name) in NAMES.iter().enumerate() {
        let ts = format!("2026-09-06 0{}:00:00", i % 8);
        if params_family {
            db.execute_params(
                r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ($1, $2, $3)"#,
                &[
                    Value::String((*name).to_string()),
                    Value::String(ts),
                    Value::Int4(i as i32),
                ],
            )
            .unwrap_or_else(|e| panic!("params INSERT of {name} failed: {e}"));
        } else {
            db.execute(&format!(
                r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ('{name}', '{ts}', {i})"#
            ))
            .unwrap_or_else(|e| panic!("text INSERT of {name} failed: {e}"));
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a text value, got {other:?}"),
    }
}

fn int_of(v: &Value) -> i64 {
    match *v {
        Value::Int2(n) => i64::from(n),
        Value::Int4(n) => i64::from(n),
        Value::Int8(n) => n,
        ref other => panic!("expected an integer, got {other:?}"),
    }
}

/// Every `name` a projecting scan can actually return, sorted.
///
/// Deliberately NOT `SELECT count(*)`: the count is answered from the PK ART
/// index length, which is the number under suspicion.
fn names_via_scan(db: &EmbeddedDatabase, params_family: bool) -> Vec<String> {
    let sql = r#"SELECT "name" FROM "_migrations""#;
    let rows = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    let mut out: Vec<String> = rows.iter().map(|r| text_of(&r.values[0])).collect();
    out.sort();
    out
}

/// `SELECT *` — the shape the report says loses rows.
fn star_row_count(db: &EmbeddedDatabase, params_family: bool) -> usize {
    let sql = r#"SELECT * FROM "_migrations""#;
    let rows = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    rows.len()
}

fn count_star(db: &EmbeddedDatabase, params_family: bool) -> i64 {
    let sql = r#"SELECT count(*) FROM "_migrations""#;
    let rows = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    int_of(&rows[0].values[0])
}

/// `WHERE "name" = <key>` — literal on the text family, bound `$1` on the params
/// family (the shape node-pg sends). Returns the number of rows found.
fn point_lookup(db: &EmbeddedDatabase, key: &str, params_family: bool) -> usize {
    let rows = if params_family {
        db.query_params(
            r#"SELECT "name" FROM "_migrations" WHERE "name" = $1"#,
            &[Value::String(key.to_string())],
        )
        .unwrap_or_else(|e| panic!("params point lookup of {key} failed: {e}"))
    } else {
        db.query(
            &format!(r#"SELECT "name" FROM "_migrations" WHERE "name" = '{key}'"#),
            &[],
        )
        .unwrap_or_else(|e| panic!("text point lookup of {key} failed: {e}"))
    };
    rows.len()
}

/// `WHERE "name" < '0004'` — the ART range-scan path.
fn range_below(db: &EmbeddedDatabase, params_family: bool) -> Vec<String> {
    let sql = r#"SELECT "name" FROM "_migrations" WHERE "name" < '0004'"#;
    let rows = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    let mut out: Vec<String> = rows.iter().map(|r| text_of(&r.values[0])).collect();
    out.sort();
    out
}

/// `WHERE "ord" < 4` — a predicate on a NON-key column, i.e. the read path that
/// still saw the reporter's "invisible" rows.
fn non_key_predicate_count(db: &EmbeddedDatabase, params_family: bool) -> usize {
    let sql = r#"SELECT "name" FROM "_migrations" WHERE "ord" < 4"#;
    let rows = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    rows.len()
}

/// `SELECT * FROM "_migrations" WHERE "name" = '<key>'` — the OTHER PK point
/// lookup: this exact shape is intercepted before the planner by
/// `EmbeddedDatabase::try_fast_select` (src/lib.rs:12905) and answered by
/// `fast_select_rows` (:13088), whose miss rule
/// (`fast_lookup_miss_is_authoritative`, :13048) reports an empty probe as "no
/// such row" for every type but NUMERIC. A text key the ART cannot resolve is
/// therefore an authoritative "not found" here.
fn star_point_lookup(db: &EmbeddedDatabase, key: &str, params_family: bool) -> usize {
    if params_family {
        // The params twin of the same pre-planner interception
        // (`try_fast_select_params`, src/lib.rs:13563) — the shape node-pg
        // actually sends. Covering only the literal arm would say nothing
        // about it: the two families are separate code.
        db.query_params(
            r#"SELECT * FROM "_migrations" WHERE "name" = $1"#,
            &[Value::String(key.to_string())],
        )
        .unwrap_or_else(|e| panic!("params `SELECT * WHERE \"name\" = $1` for {key} failed: {e}"))
        .len()
    } else {
        let sql = format!(r#"SELECT * FROM "_migrations" WHERE "name" = '{key}'"#);
        db.query(&sql, &[])
            .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
            .len()
    }
}

/// THE invariant of this issue: every read path agrees on which rows exist.
fn assert_all_read_paths_agree(db: &EmbeddedDatabase, phase: &str, params_family: bool) {
    let fam = if params_family { "params" } else { "text" };
    let mut expected: Vec<String> = NAMES.iter().map(|n| (*n).to_string()).collect();
    expected.sort();

    assert_eq!(
        names_via_scan(db, params_family),
        expected,
        "[{fam}/{phase}] a projecting scan lost rows"
    );
    assert_eq!(
        star_row_count(db, params_family),
        NAMES.len(),
        "[{fam}/{phase}] SELECT * lost rows"
    );
    assert_eq!(
        count_star(db, params_family),
        NAMES.len() as i64,
        "[{fam}/{phase}] COUNT(*) disagrees with the rows a scan returns \
         (COUNT(*) is answered from the PK ART index length)"
    );
    for name in NAMES {
        assert_eq!(
            point_lookup(db, name, params_family),
            1,
            "[{fam}/{phase}] WHERE \"name\" = '{name}' found no row, but the row is in the table"
        );
        assert_eq!(
            star_point_lookup(db, name, params_family),
            1,
            "[{fam}/{phase}] SELECT * WHERE \"name\" = '{name}' (fast_select_rows) found no row"
        );
    }
    assert_eq!(
        range_below(db, params_family),
        below_probe(),
        "[{fam}/{phase}] WHERE \"name\" < '0004' (ART range scan) lost rows"
    );
    assert_eq!(
        non_key_predicate_count(db, params_family),
        4,
        "[{fam}/{phase}] a predicate on the non-key column disagrees"
    );
}

// ===========================================================================
// 0. POSITIVE CONTROL — passes before and after any fix
// ===========================================================================

/// Proves the harness itself works: on a freshly written, never-reopened
/// database every read path already agrees, on both families. If this ever
/// fails, nothing else in this file means anything.
#[test]
fn positive_control_every_read_path_agrees_before_any_restart() {
    for params_family in [false, true] {
        let temp = TempDir::new().unwrap();
        let db = open(temp.path());
        seed(&db, params_family);
        assert_all_read_paths_agree(&db, "fresh", params_family);
        drop(db);
    }
}

// ===========================================================================
// 1. Two clean close-and-reopen cycles (locks in the shipped fixes)
// ===========================================================================

/// Text family, disk-backed, `>= 2` reopen cycles.
#[test]
fn agree_across_two_clean_reopen_cycles_text_family() {
    let temp = TempDir::new().unwrap();
    {
        let db = open(temp.path());
        seed(&db, false);
        assert_all_read_paths_agree(&db, "pre-close", false);
    }
    for cycle in 1..=2 {
        let db = open(temp.path());
        assert_all_read_paths_agree(&db, &format!("reopen-{cycle}"), false);
        // Write on every cycle: the reopen must not hand the new row an
        // already-used internal row id.
        db.execute(&format!(
            r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ('9{cycle}_after', '2026-09-07 00:00:00', 9{cycle})"#
        ))
        .unwrap();
        let after: usize = star_row_count(&db, false);
        assert_eq!(
            after,
            NAMES.len() + 1,
            "reopen-{cycle}: the post-reopen INSERT overwrote a live row instead of adding one"
        );
        assert_eq!(
            count_star(&db, false),
            after as i64,
            "reopen-{cycle}: COUNT(*) disagrees with the rows after the post-reopen INSERT"
        );
        // Put the table back to its 8-row shape for the next cycle.
        db.execute(&format!(r#"DELETE FROM "_migrations" WHERE "name" = '9{cycle}_after'"#))
            .unwrap();
        assert_all_read_paths_agree(&db, &format!("reopen-{cycle}-restored"), false);
    }
}

/// Params family (the extended protocol node-pg uses — the reporter's client).
#[test]
fn agree_across_two_clean_reopen_cycles_params_family() {
    let temp = TempDir::new().unwrap();
    {
        let db = open(temp.path());
        seed(&db, true);
        assert_all_read_paths_agree(&db, "pre-close", true);
    }
    for cycle in 1..=2 {
        let db = open(temp.path());
        assert_all_read_paths_agree(&db, &format!("reopen-{cycle}"), true);
        db.execute_params(
            r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ($1, $2, $3)"#,
            &[
                Value::String(format!("9{cycle}_after")),
                Value::String("2026-09-07 00:00:00".to_string()),
                Value::Int4(90 + cycle),
            ],
        )
        .unwrap();
        let after = star_row_count(&db, true);
        assert_eq!(
            after,
            NAMES.len() + 1,
            "reopen-{cycle}: the post-reopen parameterised INSERT overwrote a live row"
        );
        assert_eq!(
            count_star(&db, true),
            after as i64,
            "reopen-{cycle}: COUNT(*) disagrees with the rows after the post-reopen INSERT"
        );
        db.execute_params(
            r#"DELETE FROM "_migrations" WHERE "name" = $1"#,
            &[Value::String(format!("9{cycle}_after"))],
        )
        .unwrap();
        assert_all_read_paths_agree(&db, &format!("reopen-{cycle}-restored"), true);
    }
}

/// Non-ASCII and control-adjacent bytes in a text primary key, across two clean
/// reopen cycles. Ordering is deliberately NOT asserted here (that would test
/// collation, not this issue) — only that every key remains findable and that
/// COUNT(*) agrees with the rows.
#[test]
fn agree_across_two_clean_reopen_cycles_non_ascii_text_pk() {
    const ODD: [&str; 6] = ["m", "m\u{1}ctl", "m\u{1}ctl_more", "münchen", "münchen_2", "键\u{7f}x"];
    let temp = TempDir::new().unwrap();
    {
        let db = open(temp.path());
        db.execute(r#"CREATE TABLE odd_pk ("k" VARCHAR(200) PRIMARY KEY, "ord" INT NOT NULL)"#)
            .unwrap();
        for (i, k) in ODD.iter().enumerate() {
            db.execute_params(
                r#"INSERT INTO odd_pk ("k", "ord") VALUES ($1, $2)"#,
                &[Value::String((*k).to_string()), Value::Int4(i as i32)],
            )
            .unwrap_or_else(|e| panic!("insert of {k:?} failed: {e}"));
        }
    }
    for cycle in 1..=2 {
        let db = open(temp.path());
        let rows = db.query(r#"SELECT "k" FROM odd_pk"#, &[]).unwrap();
        assert_eq!(rows.len(), ODD.len(), "cycle {cycle}: a scan lost odd-key rows");
        let count = int_of(&db.query("SELECT count(*) FROM odd_pk", &[]).unwrap()[0].values[0]);
        assert_eq!(
            count,
            ODD.len() as i64,
            "cycle {cycle}: COUNT(*) (PK ART length) disagrees with the rows"
        );
        for k in ODD {
            let hits = db
                .query_params(
                    r#"SELECT "k" FROM odd_pk WHERE "k" = $1"#,
                    &[Value::String(k.to_string())],
                )
                .unwrap_or_else(|e| panic!("cycle {cycle}: lookup of {k:?} failed: {e}"))
                .len();
            assert_eq!(hits, 1, "cycle {cycle}: WHERE \"k\" = {k:?} lost its row");
        }
    }
}

// ===========================================================================
// 2. ***OPEN*** — the public checkpoint API writes snapshot markers without
//    flushing the row counters
// ===========================================================================

/// ***OPEN***: `StorageEngine::persist_index_snapshots()` (pub, documented as
/// "the explicit checkpoint API", src/storage/engine.rs:9099) writes the
/// `idxsnapv:` validity markers but never flushes `counter:{table}`. A crash
/// after it reopens with a VALID snapshot and a STALE counter — the exact state
/// `EmbeddedDatabase::drop`'s ordering comment (src/lib.rs:833-847) calls
/// unrecoverable, because `reseed_row_counter_from_max_row_id` is reachable only
/// from the scan fallback that a valid snapshot skips
/// (src/storage/catalog.rs:1637-1712).
///
/// On the current tree the post-reopen INSERT reuses row id 1 and silently
/// overwrites the OLDEST row: `SELECT *` returns 8 where 9 are expected, while
/// `COUNT(*)` reports 9 — the reporter's fingerprint, reproduced with public
/// APIs only.
#[test]
fn checkpoint_then_crash_reopen_must_not_reuse_a_row_id_text_pk() {
    // CHILD: build the pre-crash state and die without running Drop.
    if let Ok(path) = std::env::var(CRASH_CHILD_ENV) {
        let db = open(Path::new(&path));
        seed(&db, true); // parameterised INSERTs, as the report describes
                         // The public checkpoint API: writes the ART snapshot AND its validity
                         // markers. It does NOT flush the row counters.
        db.storage.persist_index_snapshots().expect("checkpoint");
        std::process::exit(0);
    }

    let temp = TempDir::new().unwrap();
    crash_via_child(
        "checkpoint_then_crash_reopen_must_not_reuse_a_row_id_text_pk",
        temp.path(),
    );

    let db = open(temp.path());
    let report = db
        .storage
        .last_index_open_report()
        .expect("an open must always leave an index-open report");
    eprintln!("gh31 open report after checkpoint+crash: {report:?}");
    // ANTI-VACUITY GUARD. Everything below is only meaningful if this open
    // actually took the snapshot fast path — that is the trap. If the snapshot
    // were rejected (index-set mismatch, marker version bump, …) the open would
    // fall into the scan rebuild, `reseed_row_counter_from_max_row_id` would run,
    // no row id would collide, and the rest of this test would pass while
    // testing NOTHING. Fail loudly instead of passing green.
    //
    // Post-fix this stays true for FIX A (the checkpoint flushes the counters
    // but still writes the snapshot) and FIX B (the snapshot load reseeds). A
    // fix that instead stops writing snapshots would legitimately change this
    // line — update it deliberately, do not delete it.
    assert!(
        report.tables_from_snapshot >= 1 && report.rows_scanned == 0,
        "the checkpoint+crash trap did not arm: the reopen was expected to load \
         `_migrations` from the ART snapshot without scanning a single row, but the \
         open report says {report:?}. Nothing below this line proves anything until \
         that is true."
    );

    // Precondition — the crash itself must not have lost anything.
    assert_all_read_paths_agree(&db, "post-crash-reopen", true);

    // The write that trips it: one more parameterised INSERT, exactly as a
    // migration runner would do on the next deploy.
    db.execute_params(
        r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ($1, $2, $3)"#,
        &[
            Value::String("0011_after_restart".to_string()),
            Value::String("2026-09-07 09:00:00".to_string()),
            Value::Int4(11),
        ],
    )
    .expect("post-reopen insert");

    // POSITIVE CONTROL inside the failing test: the new row is always readable,
    // before and after the fix. If this ever fails, the test is broken, not the
    // engine.
    assert_eq!(
        point_lookup(&db, "0011_after_restart", true),
        1,
        "control: the row just inserted must be findable"
    );

    let rows = star_row_count(&db, true);
    assert_eq!(
        rows,
        NAMES.len() + 1,
        "*** DATA LOSS *** the post-reopen INSERT reused an internal row id and \
         overwrote a live row: SELECT * returns {rows}, expected {}",
        NAMES.len() + 1
    );
    assert_eq!(
        count_star(&db, true),
        rows as i64,
        "COUNT(*) (PK ART index length) over-reports: it counts a key whose row is gone"
    );
    for name in NAMES {
        assert_eq!(
            point_lookup(&db, name, true),
            1,
            "*** GH#31 *** '{name}' is counted but unreadable: WHERE \"name\" = '{name}' \
             found nothing after the checkpoint+crash reopen"
        );
        assert_eq!(
            star_point_lookup(&db, name, true),
            1,
            "*** GH#31 *** SELECT * WHERE \"name\" = '{name}' (fast_select_rows) found nothing"
        );
    }
    assert_eq!(
        range_below(&db, true),
        below_probe(),
        "WHERE \"name\" < '0004' lost rows after the checkpoint+crash reopen"
    );
}

// ===========================================================================
// 3. ***OPEN*** — a failed close-time counter flush still writes the snapshot
// ===========================================================================

/// ***OPEN***: `EmbeddedDatabase::drop` WARNS on a failed `flush_all_row_counters()`
/// (src/lib.rs:849-856) and then writes the index snapshot anyway
/// (src/lib.rs:863-870). The documented ordering only helps when the flush
/// SUCCEEDS; when it fails the close produces exactly the combination the same
/// comment says must never exist. `set_row_counter_flush_on_close(false)` models
/// that failure with no process kill (the same knob
/// `tests/durable_index_tests.rs` uses for the crash-window tests).
///
/// Current tree: the reopen takes the snapshot fast path, never reseeds, and the
/// next INSERT overwrites the oldest row.
#[test]
fn flush_failure_at_close_must_not_leave_a_trusted_snapshot_text_pk() {
    let temp = TempDir::new().unwrap();
    {
        let db = open(temp.path());
        seed(&db, true);
        // "The counter flush failed / never ran; the checkpoint still did."
        db.storage.set_row_counter_flush_on_close(false);
    }

    let db = open(temp.path());
    let report = db
        .storage
        .last_index_open_report()
        .expect("an open must always leave an index-open report");
    eprintln!("gh31 open report after flush-less close: {report:?}");
    // ANTI-VACUITY GUARD — see the identical note in
    // `checkpoint_then_crash_reopen_must_not_reuse_a_row_id_text_pk`.
    assert!(
        report.tables_from_snapshot >= 1 && report.rows_scanned == 0,
        "the flush-less-close trap did not arm: expected a snapshot load with no row \
         scan, got {report:?}"
    );
    assert_all_read_paths_agree(&db, "post-flushless-reopen", true);

    db.execute_params(
        r#"INSERT INTO "_migrations" ("name", "appliedAt", "ord") VALUES ($1, $2, $3)"#,
        &[
            Value::String("0011_after_restart".to_string()),
            Value::String("2026-09-07 09:00:00".to_string()),
            Value::Int4(11),
        ],
    )
    .expect("post-reopen insert");

    assert_eq!(
        point_lookup(&db, "0011_after_restart", true),
        1,
        "control: the row just inserted must be findable"
    );
    assert_eq!(
        star_row_count(&db, true),
        NAMES.len() + 1,
        "*** DATA LOSS *** the post-reopen INSERT reused an internal row id"
    );
    assert_eq!(
        count_star(&db, true),
        (NAMES.len() + 1) as i64,
        "COUNT(*) over-reports after a reused row id"
    );
    for name in NAMES {
        assert_eq!(
            point_lookup(&db, name, true),
            1,
            "*** GH#31 *** '{name}' is counted but unreadable"
        );
        assert_eq!(
            star_point_lookup(&db, name, true),
            1,
            "*** GH#31 *** SELECT * WHERE \"name\" = '{name}' (fast_select_rows) found nothing"
        );
    }
}

// ===========================================================================
// 4. ***OPEN*** — nothing DETECTS or REPAIRS a diverged primary-key index
// ===========================================================================
//
// The two tests below are deliberately independent of HOW the divergence
// arose. The counter-reuse tests above prove ONE way to create it; these prove
// that once it exists — by any route, including a bug not yet found — the
// database has no way back. That matters because FIX A / FIX B (flush the
// counters in `persist_index_snapshots`, reseed on the snapshot load path)
// close the specific route above and would make a divergence test that relies
// on row-id reuse pass VACUOUSLY, still exercising neither REINDEX nor any
// detection. So the divergence is INJECTED here through the public ART API
// (`StorageEngine::art_indexes()` -> `ArtIndexManager::on_insert`,
// src/storage/art_manager.rs:2409), which is exactly the state a reused row id
// leaves behind: a PRIMARY KEY entry naming a `data:` row that is not there.

/// Put a phantom PRIMARY KEY entry into the live ART: key `name`, pointing at
/// `row_id`, with no `data:{table}:{row_id}` row behind it. This is byte-for-byte
/// the state `insert_tuple_fast` leaves when a reused row id overwrites an older
/// row (the older row's PK entry survives, its row does not) — reached here
/// without depending on the reuse bug, so these tests keep their teeth after
/// FIX A and FIX B land.
fn inject_phantom_pk_entry(db: &EmbeddedDatabase, name: &str, row_id: u64) {
    let mut cols: HashMap<String, Value> = HashMap::new();
    cols.insert("name".to_string(), Value::String(name.to_string()));
    db.storage
        .art_indexes()
        .on_insert("_migrations", row_id, &cols)
        .unwrap_or_else(|e| panic!("could not inject phantom PK entry {name:?}: {e}"));
}

/// Arms and PROVES the divergence, so neither test below can pass vacuously:
/// after this returns, `COUNT(*)` reports one more row than any scan can
/// produce. Returns the number of rows a scan actually returns.
fn arm_divergence(db: &EmbeddedDatabase) -> usize {
    inject_phantom_pk_entry(db, "0099_phantom", 9_999);

    let rows = star_row_count(db, true);
    assert_eq!(
        rows,
        NAMES.len(),
        "setup: injecting an index entry must not change how many rows exist"
    );
    assert_eq!(
        count_star(db, true),
        (NAMES.len() + 1) as i64,
        "setup: the divergence did not arm — COUNT(*) is not answered from the PK ART \
         index length any more, so this test needs rewriting against whatever now \
         answers it"
    );
    rows
}

/// POSITIVE CONTROL for section 4: passes before and after any fix. A diverged
/// index must never make a row that IS present unreadable — if this ever fails,
/// the injection helper is broken, not the engine.
fn assert_real_rows_still_readable(db: &EmbeddedDatabase, phase: &str) {
    assert_eq!(
        names_via_scan(db, true),
        {
            let mut e: Vec<String> = NAMES.iter().map(|n| (*n).to_string()).collect();
            e.sort();
            e
        },
        "control [{phase}]: every real row must still be scannable"
    );
    for name in NAMES {
        assert_eq!(
            point_lookup(db, name, true),
            1,
            "control [{phase}]: real row '{name}' must still be findable by its key"
        );
    }
}

/// ***OPEN***: `REINDEX` — the one PostgreSQL surface that exists for exactly
/// this — is an accepted NO-OP (`try_handle_reindex_statement`, src/lib.rs:2559,
/// whose comment claims "Nano's index storage has no user-visible rebuild need
/// to satisfy"; this issue is the counter-example). It returns success, so an
/// operator has every reason to believe the index was rebuilt, and nothing was.
///
/// After the fix, `REINDEX TABLE "_migrations"` must rebuild the table's ART
/// indexes from the rows, which drops the phantom entry and makes `COUNT(*)`
/// agree with a scan again.
#[test]
fn reindex_must_repair_a_diverged_pk_index() {
    let temp = TempDir::new().unwrap();
    let db = open(temp.path());
    seed(&db, true);
    assert_all_read_paths_agree(&db, "pre-injection", true);

    let rows = arm_divergence(&db);
    assert_real_rows_still_readable(&db, "after injection");

    // REINDEX must be ACCEPTED (it already is) and must actually rebuild.
    db.execute(r#"REINDEX TABLE "_migrations""#)
        .expect("REINDEX TABLE must be accepted");

    assert_eq!(
        count_star(&db, true),
        rows as i64,
        "*** UNREPAIRABLE *** REINDEX TABLE reported success and repaired nothing: \
         COUNT(*) still reports {} rows while a scan returns {rows}. \
         `try_handle_reindex_statement` (src/lib.rs:2559) returns Ok(Some(0)) \
         without touching a single index.",
        count_star(&db, true)
    );
    assert_eq!(
        point_lookup(&db, "0099_phantom", true),
        0,
        "the phantom key must not resolve after a rebuild"
    );
    // Control again: the repair must not have eaten any real row.
    assert_real_rows_still_readable(&db, "after REINDEX");
    assert_all_read_paths_agree(&db, "after REINDEX", true);
}

/// ***OPEN***: a clean close+reopen does not repair it either — it PRESERVES it.
/// The close-time checkpoint exports the phantom entry verbatim
/// (`export_table_snapshot`, src/storage/art_manager.rs:382) and the next open
/// bulk-loads it straight back (`load_index_entries`, :414;
/// `Catalog::load_table_from_snapshot`, src/storage/catalog.rs:1746) without
/// consulting a single `data:` key. So the database keeps reporting a row count
/// it cannot produce rows for, across every restart, forever — which is what
/// "six hours and two container restarts" looked like to the reporter.
///
/// After the fix the open must DETECT that the snapshot's PK entries do not
/// match the rows and fall back to the (always-correct) scan rebuild.
#[test]
fn a_diverged_pk_index_must_not_survive_a_clean_restart() {
    let temp = TempDir::new().unwrap();
    {
        let db = open(temp.path());
        seed(&db, true);
        arm_divergence(&db);
        assert_real_rows_still_readable(&db, "before clean close");
        // Clean close: counters flushed, then the checkpoint runs and snapshots
        // the ART exactly as it stands — phantom entry included.
    }

    let db = open(temp.path());
    let report = db.storage.last_index_open_report();
    eprintln!("gh31 open report after a diverged clean close: {report:?}");

    // CONTROL first — a repair must never cost a real row.
    assert_real_rows_still_readable(&db, "after clean reopen");

    let rows = star_row_count(&db, true);
    assert_eq!(
        rows,
        NAMES.len(),
        "control: the reopen must not have lost or invented rows"
    );
    assert_eq!(
        count_star(&db, true),
        rows as i64,
        "*** UNREPAIRABLE *** a clean close+reopen round-tripped the phantom PK entry: \
         COUNT(*) = {}, rows = {rows}. The checkpoint exported it and the open \
         bulk-loaded it back without ever looking at a row.",
        count_star(&db, true)
    );
    assert_eq!(
        point_lookup(&db, "0099_phantom", true),
        0,
        "the phantom key must not survive a restart"
    );
    assert_all_read_paths_agree(&db, "after clean reopen", true);
}
