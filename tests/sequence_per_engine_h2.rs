//! sprinter d15933f528b0 + 064a59d8fb7c — sequence state belongs to ONE
//! DATABASE, not to the process.
//!
//! Through v4.39.0 `src/sql/sequences.rs` held both halves of its state in
//! process-globals:
//!
//!   * `static PERSIST: OnceLock<Mutex<Option<Weak<StorageEngine>>>>`, written
//!     by `install_persistence` with an UNCONDITIONAL overwrite at every
//!     `EmbeddedDatabase` open, so the most recently constructed database won
//!     the slot for the whole process; and
//!   * `static STORE: OnceLock<Mutex<HashMap<String, Arc<SeqRuntime>>>>`, keyed
//!     by BARE SEQUENCE NAME, so two databases with a sequence of the same name
//!     shared one runtime.
//!
//! Every test below is a REPRO of that code's failure and passes on the
//! per-namespace replacement — with one deliberate exception,
//! `create_sequence_options_are_honoured_on_a_single_database`, which is the
//! CONTROL for sprinter 064a59d8fb7c: it uses one database and no second engine
//! at all, so it passes either way, and its job is to show that `START WITH`
//! (and the other clauses that arrive through the same options list) has no
//! defect of its own. The failure each repro reproduces is named in its own doc
//! comment. They are deterministic when the file is run on its own; run
//! alongside other suites in the same binary, another test's live database can
//! mask the OLD failure (never the new behaviour), which is why the repros live
//! in a file of their own.
//!
//! Sequence names here are deliberately FIXED and deliberately REUSED across
//! the two databases inside a test. Every fixed sequence name in the corpus was
//! a latent cross-suite hazard while the store was name-keyed — the drizzle
//! suite works around it with UUID suffixes to this day — and the point of
//! these tests is to prove the hazard is gone rather than to keep dodging it.
//! For the same reason this file takes NO serialization lock: if per-database
//! namespaces work, concurrent suites cannot reach each other's sequences.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---- helpers ----------------------------------------------------------------

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nano_seq_h2_{tag}_{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `SELECT nextval('<name>')` through the full SQL stack, as a client sees it.
fn nextval(db: &EmbeddedDatabase, name: &str) -> i64 {
    scalar(db, &format!("SELECT nextval('{name}')"))
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match rows[0].values[0] {
        Value::Int8(v) => v,
        Value::Int4(v) => v as i64,
        ref other => panic!("{sql} returned a non-integer: {other:?}"),
    }
}

// ---- 1. two live databases, ONE sequence name -------------------------------

/// Two `EmbeddedDatabase`s in one process, each with a sequence of the SAME
/// name, must run two INDEPENDENT streams.
///
/// Old failure: both `CREATE SEQUENCE`s wrote to their own catalogs, but
/// `runtime_for` resolved the single `PERSIST` slot — the LAST database opened —
/// so the first `nextval` on database A built its runtime from database B's
/// catalog and cached it in the one name-keyed `STORE`. Database B's own
/// `nextval` then continued A's stream: it returned 2, not 1.
#[test]
fn two_databases_do_not_share_a_sequence_name() {
    let db_a = EmbeddedDatabase::new_in_memory().unwrap();
    let db_b = EmbeddedDatabase::new_in_memory().unwrap();

    // Same name in both. No UUID suffix: that IS the assertion.
    db_a.execute("CREATE SEQUENCE h2_shared CACHE 1").unwrap();
    db_b.execute("CREATE SEQUENCE h2_shared CACHE 1").unwrap();

    assert_eq!(nextval(&db_a, "h2_shared"), 1, "A's first value");
    assert_eq!(nextval(&db_a, "h2_shared"), 2, "A's second value");
    assert_eq!(nextval(&db_a, "h2_shared"), 3, "A's third value");

    // B has never been advanced, so B starts at 1 — it does not continue A's
    // stream, and A's three values did not come out of B's store.
    assert_eq!(
        nextval(&db_b, "h2_shared"),
        1,
        "database B's sequence continued database A's stream"
    );
    assert_eq!(nextval(&db_b, "h2_shared"), 2, "B's second value");

    // And A is unmoved by B's two calls.
    assert_eq!(nextval(&db_a, "h2_shared"), 4, "B's nextval advanced A");
}

// ---- 2. setval is not visible across databases ------------------------------

/// `setval` on one database must not move the other's sequence of the same
/// name, and must not flush its high-water into the other's store.
///
/// Old failure: `try_setval` fsynced through the single `PERSIST` handle, so
/// the high-water landed in whichever database had opened last, and the shared
/// name-keyed runtime made the jump visible to BOTH.
#[test]
fn setval_on_one_database_is_invisible_to_the_other() {
    let db_a = EmbeddedDatabase::new_in_memory().unwrap();
    let db_b = EmbeddedDatabase::new_in_memory().unwrap();

    db_a.execute("CREATE SEQUENCE h2_setval CACHE 1").unwrap();
    db_b.execute("CREATE SEQUENCE h2_setval CACHE 1").unwrap();
    assert_eq!(nextval(&db_a, "h2_setval"), 1);
    assert_eq!(nextval(&db_b, "h2_setval"), 1);

    assert_eq!(scalar(&db_a, "SELECT setval('h2_setval', 9000)"), 9000);

    // A resumes past the setval point...
    assert_eq!(nextval(&db_a, "h2_setval"), 9001, "A did not resume past setval");
    // ...and B is untouched.
    assert_eq!(
        nextval(&db_b, "h2_setval"),
        2,
        "database A's setval leaked into database B"
    );
}

// ---- 3. the observed flake --------------------------------------------------

/// The failure actually observed in a 335-suite run:
/// `session_scoping_batch_g2::currval_agrees_with_lastval_and_survives_a_rollback`
/// died on `setval requires storage context` and passed 15/0 when run alone —
/// same binary, pass and fail.
///
/// Old failure: an unrelated `EmbeddedDatabase` opened later in the binary
/// overwrote `PERSIST`; when it was dropped the `Weak` no longer upgraded, and
/// `try_setval` on a perfectly healthy database raised
/// `setval requires storage context` (`sequences.rs`, the `ok_or_else` in the
/// `!rt.volatile` branch).
#[test]
fn setval_survives_another_database_being_opened_and_dropped() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE h2_ghost_setval CACHE 1").unwrap();
    assert_eq!(nextval(&db, "h2_ghost_setval"), 1);

    {
        // A second database opens (this used to steal the one global slot) and
        // is then dropped (which used to make the slot un-upgradeable).
        let other = EmbeddedDatabase::new_in_memory().unwrap();
        other.execute("CREATE TABLE h2_unrelated (id INT)").unwrap();
    }

    assert_eq!(
        scalar(&db, "SELECT setval('h2_ghost_setval', 42)"),
        42,
        "setval failed on a healthy database after an unrelated database was dropped"
    );
    assert_eq!(nextval(&db, "h2_ghost_setval"), 43);
}

/// The SAME hazard on the `nextval` refill path, which the brief calls out
/// separately: `persist_high_water` resolves the handle too, and raises
/// `nextval requires storage context` when it cannot. `CACHE 1` forces every
/// call through the refill, so this is not merely the post-CREATE first call.
#[test]
fn nextval_refill_survives_another_database_being_opened_and_dropped() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE h2_ghost_nextval CACHE 1").unwrap();
    assert_eq!(nextval(&db, "h2_ghost_nextval"), 1);

    {
        let other = EmbeddedDatabase::new_in_memory().unwrap();
        other.execute("CREATE TABLE h2_unrelated2 (id INT)").unwrap();
    }

    // Every one of these takes the refill path (cache 1) and therefore
    // `persist_high_water`.
    assert_eq!(nextval(&db, "h2_ghost_nextval"), 2, "refill failed after the drop");
    assert_eq!(nextval(&db, "h2_ghost_nextval"), 3);
    assert_eq!(nextval(&db, "h2_ghost_nextval"), 4);
}

// ---- 4. sprinter 064a58...: settling START WITH ------------------------------

/// The `CREATE SEQUENCE s START WITH 500` → `nextval` → **1** report.
///
/// This is the (a) half of the question: the clause was parsed and persisted
/// correctly, but the `nextval` that followed resolved the WRONG ENGINE. With a
/// second database opened after the CREATE, the old `runtime_for` read that
/// database's catalog, found no such sequence, D7-auto-vivified a default
/// (start 1) sequence IN IT, and returned 1 — on a fresh database whose
/// catalog plainly said 500.
#[test]
fn start_with_is_honoured_while_another_database_is_open() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE h2_start_with START WITH 500 CACHE 1")
        .unwrap();

    // Opened AFTER the CREATE: this is the database the old single slot handed
    // the following `nextval` to. It gets a sequence of its OWN, which is the
    // non-vacuity guard for the absence probe at the end — without it, an empty
    // `pg_sequences` would "prove" the absence for the wrong reason.
    let newer = EmbeddedDatabase::new_in_memory().unwrap();
    newer.execute("CREATE SEQUENCE h2_start_with_other").unwrap();

    assert_eq!(
        nextval(&db, "h2_start_with"),
        500,
        "START WITH was answered from another database's catalog"
    );
    assert_eq!(nextval(&db, "h2_start_with"), 501);

    // The newer database never had `h2_start_with`, and must not have acquired
    // one by auto-vivification on someone else's behalf.
    let names: Vec<String> = newer
        .query("SELECT sequencename FROM pg_sequences", &[])
        .unwrap()
        .iter()
        .map(|t| match t.values[0] {
            Value::String(ref s) => s.clone(),
            ref other => panic!("sequencename was not text: {other:?}"),
        })
        .collect();
    assert!(
        names.iter().any(|n| n == "h2_start_with_other"),
        "pg_sequences on the newer database is empty — the absence probe below would be vacuous"
    );
    assert!(
        !names.iter().any(|n| n == "h2_start_with"),
        "the other database auto-vivified a sequence it was never asked for: {names:?}"
    );
}

/// The (b) half of the same question, isolated from the namespace bug entirely:
/// ONE database, no second engine anywhere, every `CREATE SEQUENCE` option that
/// arrives through the same `SequenceOptions` list as `START WITH`.
///
/// If this passes, `START WITH` has no defect of its own and 064a59d8fb7c is
/// answered (a).
#[test]
fn create_sequence_options_are_honoured_on_a_single_database() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE h2_opts START WITH 500 INCREMENT BY 7 MINVALUE 100 MAXVALUE 520 CYCLE CACHE 1")
        .unwrap();

    assert_eq!(nextval(&db, "h2_opts"), 500, "START WITH");
    assert_eq!(nextval(&db, "h2_opts"), 507, "INCREMENT BY");
    assert_eq!(nextval(&db, "h2_opts"), 514, "INCREMENT BY");
    // 521 would exceed MAXVALUE 520, so CYCLE wraps to MINVALUE.
    assert_eq!(nextval(&db, "h2_opts"), 100, "MAXVALUE + CYCLE -> MINVALUE");

    // START WITH alone, with every other clause left at its default.
    db.execute("CREATE SEQUENCE h2_opts_bare START WITH 500").unwrap();
    assert_eq!(nextval(&db, "h2_opts_bare"), 500, "bare START WITH");

    // A descending sequence, whose START is the MAXVALUE end.
    db.execute("CREATE SEQUENCE h2_opts_desc INCREMENT BY -1 MINVALUE -3 MAXVALUE -1 START WITH -1 CACHE 1")
        .unwrap();
    assert_eq!(nextval(&db, "h2_opts_desc"), -1);
    assert_eq!(nextval(&db, "h2_opts_desc"), -2);
}

// ---- 5. durable independence across a reopen --------------------------------

/// Two databases on two data dirs, one sequence name, must keep INDEPENDENT
/// durable state across a close and reopen.
///
/// Old failure: the durable writes themselves went through the shared handle,
/// so A's high-water could be fsynced into B's `meta:seqstate:` record; after a
/// reopen B resumed from a value it had never served, and A resumed from one it
/// had.
#[test]
fn each_database_keeps_its_own_durable_state_across_a_reopen() {
    let dir_a = scratch_dir("a");
    let dir_b = scratch_dir("b");

    {
        let db_a = EmbeddedDatabase::new(&dir_a).unwrap();
        let db_b = EmbeddedDatabase::new(&dir_b).unwrap();

        db_a.execute("CREATE SEQUENCE h2_durable CACHE 1").unwrap();
        db_b.execute("CREATE SEQUENCE h2_durable CACHE 1").unwrap();

        assert_eq!(nextval(&db_a, "h2_durable"), 1);
        assert_eq!(nextval(&db_a, "h2_durable"), 2);
        assert_eq!(nextval(&db_b, "h2_durable"), 1);

        // A jumps far ahead; B must not follow it, then or after the reopen.
        assert_eq!(scalar(&db_a, "SELECT setval('h2_durable', 100000)"), 100000);
    }

    {
        // Fresh attach to both dirs — a fresh runtime map, everything read back
        // from each dir's own durable records.
        let db_a = EmbeddedDatabase::new(&dir_a).unwrap();
        let db_b = EmbeddedDatabase::new(&dir_b).unwrap();

        let a = nextval(&db_a, "h2_durable");
        assert!(a > 100000, "A did not resume past its own setval: {a}");

        let b = nextval(&db_b, "h2_durable");
        assert!(b > 1 && b < 1000, "B resumed from database A's durable high-water: {b}");
    }

    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();
}

// ---- 6. introspection follows the same namespace ----------------------------

/// `pg_sequences.last_value` is served from the in-memory runtime map
/// (`sequences::peek_last_served`). It has to read the CALLING database's map,
/// or a client asking "where is my sequence now" is answered with another
/// tenant's counter.
#[test]
fn pg_sequences_last_value_is_per_database() {
    let db_a = EmbeddedDatabase::new_in_memory().unwrap();
    let db_b = EmbeddedDatabase::new_in_memory().unwrap();

    db_a.execute("CREATE SEQUENCE h2_introspect CACHE 1").unwrap();
    db_b.execute("CREATE SEQUENCE h2_introspect CACHE 1").unwrap();

    for _ in 0..5 {
        nextval(&db_a, "h2_introspect");
    }
    assert_eq!(nextval(&db_b, "h2_introspect"), 1);

    let a_last = scalar(
        &db_a,
        "SELECT last_value FROM pg_sequences WHERE sequencename = 'h2_introspect'",
    );
    let b_last = scalar(
        &db_b,
        "SELECT last_value FROM pg_sequences WHERE sequencename = 'h2_introspect'",
    );
    assert_eq!(a_last, 5, "A's last_value");
    assert_eq!(b_last, 1, "B reported database A's last_value");
}
