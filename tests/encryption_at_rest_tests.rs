//! Encryption AT REST — proof that the DATABASE calls the cipher, not merely
//! that the cipher works.
//!
//! # Why this file exists
//!
//! `crypto::{encrypt, decrypt}` has its own unit coverage: give it bytes, get
//! different bytes back, decrypt them, compare. That establishes AES-256-GCM is
//! wired up correctly. It says nothing about whether an
//! `INSERT INTO t VALUES (...)` reaches that function. The tests here answer
//! that second question, and only that one. (`tests/encryption_tests.rs` is
//! `#![cfg(feature = "internal-tests")]`, which is not in the default feature
//! set, so it compiles to an empty binary and reports `ok. 0 passed` — which
//! this repo's CLAUDE.md instructs us to treat as a gate FAILURE, not a pass.)
//!
//! Every assertion here is therefore end-to-end through
//! `EmbeddedDatabase::with_config` — the real user-facing path — and the
//! load-bearing ones inspect the on-disk bytes.
//!
//! # The property under test
//!
//! **The on-disk form of a stored value is a function of its KEY, not of which
//! function wrote it.** `src/storage/tde.rs` owns that rule in one place, in
//! both directions (`seal*` / `open*`), for the `data:`, `v:`, `counter:`,
//! `wal:entries:`, `bdata:` and `delta:` keyspaces. `StorageEngine`,
//! `Transaction` (its commit `WriteBatch`), `SnapshotManager` (the version chain
//! and the fast autocommit `data:` write), `WriteAheadLog`, `BranchManager` and
//! the MV `DeltaTracker` all build writes against the same `Arc<rocksdb::DB>`,
//! so a rule that lived on only one of them would be a per-route opt-in.
//!
//! # The keyspace walk
//!
//! `no_value_in_a_sealed_keyspace_is_readable_as_plaintext` opens the closed
//! data directory with RocksDB directly and visits EVERY stored key/value pair.
//! For every key under one of the prefixes `is_row_value_key` seals it requires
//! the value to authenticate under the database's key; for every key outside
//! that set it requires the value not to contain a row marker. A marker-scan of
//! the files can only say "this string is not on disk"; the walk says WHICH key
//! holds a value in the wrong form, and it covers a write route nobody thought
//! to name — including one added after this file was written.
//!
//! # Where the rule stops
//!
//! Three boundaries are asserted here rather than assumed, because an unpinned
//! known limit is indistinguishable from an unnoticed one. For a column with a
//! non-default `STORAGE` mode, `data:` holds a REFERENCE and the payload lives
//! in a sidecar that `is_row_value_key` excludes; each of the three modes has a
//! test pinning that actual behaviour:
//!
//!   * `a_content_addressed_column_stores_its_payload_outside_the_seal` (`cas:`)
//!   * `a_dictionary_column_stores_its_values_outside_the_seal` (`dict:`)
//!   * `a_columnar_column_stores_its_values_outside_the_seal` (`col:`)
//!
//! # Why a round-trip test cannot substitute for the byte scan
//!
//! Write a row, read it back, compare — that passes whether or not anything was
//! encrypted, because every reader in the engine either decrypts or reads raw.
//! A plaintext write paired with a plaintext read is entirely self-consistent
//! and completely silent. The only assertion that can tell the two apart is to
//! open the data directory and look for the plaintext.
//!
//! `assert_absent` scans EVERY regular file under the data directory — SSTs,
//! MANIFEST, and the RocksDB `.log` write-ahead log, since a database this
//! small may still live entirely in the log — for a marker string that appears
//! verbatim in the bincode encoding of a row (bincode writes a `String` as an
//! 8-byte length followed by its raw UTF-8 bytes).
//!
//! Two things make that scan meaningful rather than vacuous:
//!
//!   * Block compression is turned OFF in every configuration here, so a
//!     literal byte search cannot be defeated by a row being compressed out of
//!     recognisable shape.
//!   * `plaintext_control_proves_the_scan_can_see_a_row` runs the identical
//!     workload with encryption DISABLED and requires the marker to be FOUND.
//!     Without that control, a scan that quietly looked in the wrong directory
//!     would make every ciphertext assertion below pass for the wrong reason.
//!
//! # Feature gating
//!
//! `encryption` is in the DEFAULT feature set (`Cargo.toml`:
//! `default = ["encryption", "vector-search", "ring-crypto", "ha-tier1"]`), so
//! these run under the standard `cargo test --tests` gate. This file is
//! deliberately NOT gated at module level: the control and the
//! encryption-disabled tests are ungated, so the binary can never degrade into
//! a silent `0 passed; 0 failed` result even under `--no-default-features`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::config::CompressionType;
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::path::{Path, PathBuf};

/// A string that appears verbatim in the bincode encoding of a row, and that
/// cannot plausibly occur anywhere else in a RocksDB file.
const MARKER: &str = "QZX-TDE-PLAINTEXT-CANARY-8814";
/// Used only as an UPDATE's pre-image, so the MVCC version chain can be
/// asserted separately from the live row image.
const OLD_MARKER: &str = "QZX-TDE-PREIMAGE-CANARY-4471";
/// Written by an UNENCRYPTED session into a directory that is later reopened
/// WITH encryption — the mixed-format ("existing field database") simulation.
#[cfg(feature = "encryption")]
const LEGACY_MARKER: &str = "QZX-TDE-LEGACY-PLAINTEXT-2290";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn scratch_dir(tag: &str) -> PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nano_tde_at_rest_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Every regular file under `dir`, recursively.
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// True when `haystack` contains `needle` as a contiguous byte run.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Files under `dir` whose raw bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let needle = needle.as_bytes();
    let mut hits = Vec::new();
    for path in all_files(dir) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if contains_bytes(&bytes, needle) {
            hits.push(path);
        }
    }
    hits
}

/// Visit every key/value pair a CLOSED data directory holds, in key order, and
/// return how many were seen.
///
/// This opens the directory with RocksDB itself rather than through
/// `StorageEngine`, which is the point: every read the engine offers applies the
/// storage boundary's decode, so no engine API can show what is actually
/// STORED. The handle is read-only (it takes no LOCK and writes nothing) and the
/// options are RocksDB defaults — a full `IteratorMode::Start` walk needs no
/// prefix extractor, and per-SST settings come from the files themselves.
///
/// The caller must have dropped its `EmbeddedDatabase` first: a read-only open
/// replays the RocksDB write-ahead log into an in-memory memtable, so pairs that
/// have not been flushed to an SST are visited too, but only those the writing
/// session already committed.
fn for_each_stored_pair(dir: &Path, mut visit: impl FnMut(&[u8], &[u8])) -> usize {
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    let db = rocksdb::DB::open_for_read_only(&opts, dir, false)
        .expect("the closed data directory must open read-only for the keyspace walk");

    let mut total = 0usize;
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        let (key, value) = item.expect("iterating the on-disk keyspace must not fail");
        total += 1;
        visit(&key, &value);
    }
    total
}

fn cell(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Render a result set to a comparable, order-preserving form.
fn render(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .map(|t| t.values.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

/// The DEFAULT configuration: encryption DISABLED. This is the path the perf
/// gate measures and the one that must stay byte-identical.
fn plaintext_config(dir: &Path) -> Config {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    // The on-disk assertions read stored bytes directly, so block compression
    // must be off — otherwise a plaintext row could be compressed out of a
    // literal byte search and the scan would report "ciphertext" for a
    // database that is nothing of the kind. The encrypted configuration uses
    // the same setting, so the control and the subject are comparable.
    config.storage.compression = CompressionType::None;
    // The logical WAL is covered by its OWN test rather than by these, because
    // its entries are removed by the replay-and-truncate that runs at open —
    // so a test that reopens the directory (most of the ones below do) would be
    // asserting about a keyspace whose contents change underneath it. The WAL
    // keyspace is a row keyspace like any other and is asserted, with the
    // shipped `wal_enabled = true` default, in
    // `the_logical_wal_stores_rows_as_ciphertext` / its plaintext control.
    //
    // Turning it off here does NOT change which write route the statements
    // below take. On a standalone node with the default
    // `storage.logical_wal_per_statement = false`, `fast_dml_requires_logical_wal()`
    // returns false either way, so the DEFAULT batched autocommit arm is still
    // the arm under test.
    config.storage.wal_enabled = false;
    config.encryption.enabled = false;
    config
}

/// The `canary` table as it stands on `main` once
/// [`write_rows_through_every_route`] has run, in `ORDER BY id` order.
///
/// Spelled out per row rather than counted: a count passes for a route that
/// wrote the wrong value, and it cannot say WHICH route stopped working.
/// Row 9 is absent because route (8) deleted it, and row 11 is absent because
/// route (10) wrote it on a branch.
fn expected_canary_notes() -> Vec<String> {
    vec![
        format!("1|{MARKER}-autocommit"),
        format!("2|{MARKER}-batch-a"),
        format!("3|{MARKER}-batch-b"),
        format!("4|{MARKER}-txn"),
        format!("5|{MARKER}-updated"),
        format!("6|{MARKER}-params-updated"),
        format!("7|{MARKER}-onconflict"),
        format!("8|{MARKER}-insert-select"),
        format!("10|{MARKER}-mv-base"),
    ]
}

/// Drive a marker value through EVERY row-write route this build exposes to an
/// embedded caller, and leave the database on `main`.
///
/// The routes, numbered as they appear in the body. Each reaches RocksDB
/// through a different writer, and the stored form of a value is a property of
/// its KEY, so each one is a separate opportunity for the two to disagree:
///
///   1. single-row autocommit `INSERT` — `insert_tuple_fast`, whose only
///      `data:` write on stock defaults (`time_travel_enabled = true`) is
///      `SnapshotManager::write_data_version_and_register_snapshot`;
///   2. multi-row `INSERT ... VALUES` — `insert_prepared_tuples_fast_batch`;
///   3. explicit transaction commit — `Transaction::commit`'s `WriteBatch`;
///   4. autocommit `UPDATE`, which additionally writes the pre-image into the
///      `v:` chain — a full second copy of the row (`OLD_MARKER`);
///   5. the params / extended family — `execute_params`, the family every
///      driver on the wire protocols resolves to, and a code path distinct from
///      the literal-SQL family used by routes 1-4;
///   6. `INSERT ... ON CONFLICT DO UPDATE` — the upsert arm, which reads the
///      conflicting row back and rewrites it;
///   7. `INSERT ... SELECT` — rows sourced from a scan rather than from
///      literals, so they arrive as decoded `Tuple`s from another table;
///   8. `DELETE`, which keeps the removed row in the `v:` chain and in a
///      `delta:` record;
///   9. bulk load — `StorageEngine::direct_bulk_load`, which builds its own
///      `WriteBatch` and bypasses MVCC, the logical WAL and the row cache;
///  10. a write on a NON-`main` BRANCH, which lands in the `bdata:` overlay;
///  11. a materialized-view refresh, which re-materializes rows through
///      `StorageEngine::insert_tuple` (into the MV's own `data:` table) and
///      records `delta:` entries;
///  12. `ALTER TABLE ... RENAME TO`, which stages a synthesized schema and row
///      counter and moves every row key in one batch.
///
/// COPY NOTE. `COPY ... FROM STDIN` is reachable ONLY through the PostgreSQL
/// wire protocol (`src/protocol/postgres/copy.rs` parses it;
/// `EmbeddedDatabase::copy_bulk_insert` is `pub(crate)`), and it is not part of
/// the SQL surface an embedded caller can reach — so an in-process test cannot
/// issue one without standing up a server. It does not need to: after its
/// decode, CHECK and FK phases, `copy_bulk_insert` performs its single storage
/// write with
/// `self.storage.insert_prepared_tuples_fast_batch(table_name, prepared, &spec.schema)`
/// (`src/lib.rs`) — the very same call, with the same argument shape, that the
/// multi-row `INSERT ... VALUES` fast batch makes. Route (2) therefore exercises
/// COPY's storage boundary exactly, and route (9) covers the other bulk writer,
/// which builds its batch itself.
///
/// Every route asserts its own effect before the next one runs, so a route that
/// silently stops writing fails HERE, by name, instead of quietly shrinking the
/// surface that the on-disk assertions in the callers cover.
fn write_rows_through_every_route(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE canary (id INT PRIMARY KEY, note TEXT)")
        .expect("create table");

    // (1) single-row autocommit INSERT — the DEFAULT route.
    assert_eq!(
        db.execute(&format!("INSERT INTO canary VALUES (1, '{MARKER}-autocommit')"))
            .expect("autocommit insert"),
        1,
        "route 1 (autocommit INSERT) must write exactly one row"
    );

    // (2) multi-row INSERT ... VALUES — the fast batch route, shared verbatim
    //     with COPY (see the COPY NOTE above).
    assert_eq!(
        db.execute(&format!(
            "INSERT INTO canary VALUES (2, '{MARKER}-batch-a'), (3, '{MARKER}-batch-b')"
        ))
        .expect("multi-row insert"),
        2,
        "route 2 (multi-row INSERT ... VALUES) must write two rows"
    );

    // (3) explicit transaction commit — the commit WriteBatch route.
    {
        let tx = db.begin_transaction().expect("begin");
        tx.execute(&format!("INSERT INTO canary VALUES (4, '{MARKER}-txn')"))
            .expect("txn insert");
        tx.commit().expect("commit");
    }

    // (4) UPDATE — leaves the PRE-IMAGE in the MVCC `v:` chain.
    db.execute(&format!("INSERT INTO canary VALUES (5, '{OLD_MARKER}')"))
        .expect("pre-image insert");
    assert_eq!(
        db.execute(&format!("UPDATE canary SET note = '{MARKER}-updated' WHERE id = 5"))
            .expect("update"),
        1,
        "route 4 (autocommit UPDATE) must update exactly one row"
    );

    // (5) the params / extended family — `execute_params`, not `execute`.
    assert_eq!(
        db.execute_params(
            "INSERT INTO canary VALUES ($1, $2)",
            &[Value::Int4(6), Value::String(format!("{MARKER}-params"))],
        )
        .expect("params insert"),
        1,
        "route 5 (params INSERT) must write exactly one row"
    );
    assert_eq!(
        db.execute_params(
            "UPDATE canary SET note = $1 WHERE id = $2",
            &[Value::String(format!("{MARKER}-params-updated")), Value::Int4(6)],
        )
        .expect("params update"),
        1,
        "route 5 (params UPDATE) must update exactly one row"
    );

    // (6) INSERT ... ON CONFLICT DO UPDATE — the upsert arm.
    db.execute(&format!("INSERT INTO canary VALUES (7, '{MARKER}-preconflict')"))
        .expect("seed the row the upsert will collide with");
    db.execute(&format!(
        "INSERT INTO canary VALUES (7, '{MARKER}-onconflict') ON CONFLICT (id) DO UPDATE SET note = EXCLUDED.note"
    ))
    .expect("upsert");

    // (7) INSERT ... SELECT — rows that arrive from a scan of another table.
    db.execute("CREATE TABLE canary_src (id INT PRIMARY KEY, note TEXT)")
        .expect("create the INSERT ... SELECT source table");
    db.execute(&format!("INSERT INTO canary_src VALUES (8, '{MARKER}-insert-select')"))
        .expect("seed the source table");
    assert_eq!(
        db.execute("INSERT INTO canary (id, note) SELECT id, note FROM canary_src")
            .expect("insert ... select"),
        1,
        "route 7 (INSERT ... SELECT) must write exactly one row"
    );

    // (8) DELETE — the removed row survives in the `v:` chain and in a `delta:`
    //     record, which is why its contents carry the marker too.
    db.execute(&format!("INSERT INTO canary VALUES (9, '{MARKER}-deleted')"))
        .expect("insert the row that will be deleted");
    assert_eq!(
        db.execute("DELETE FROM canary WHERE id = 9").expect("delete"),
        1,
        "route 8 (DELETE) must remove exactly one row"
    );

    // (9) bulk load — its own `WriteBatch`, into its own table so that the row
    //     ids it assigns cannot collide with the counter the routes above use.
    db.execute("CREATE TABLE canary_bulk (id INT PRIMARY KEY, note TEXT)")
        .expect("create the bulk-load table");
    let bulk_row = bincode::serialize(&Tuple::with_row_id(
        vec![Value::Int4(1), Value::String(format!("{MARKER}-bulk"))],
        1,
    ))
    .expect("serialize the bulk-loaded row");
    let loaded = db
        .storage
        .direct_bulk_load("canary_bulk", vec![(1u64, bulk_row)], 100_000, true)
        .expect("bulk load");
    assert_eq!(loaded.rows_loaded, 1, "route 9 (bulk load) must report one row loaded");
    assert_eq!(
        render(
            &db.query("SELECT id, note FROM canary_bulk", &[])
                .expect("read the bulk-loaded row back")
        ),
        vec![format!("1|{MARKER}-bulk")],
        "route 9 (bulk load) must produce a readable row"
    );

    // (10) a write on a NON-`main` branch — the `bdata:` overlay keyspace.
    db.execute("CREATE BRANCH dev AS OF NOW").expect("create branch");
    db.execute("USE BRANCH dev").expect("use branch");
    db.execute(&format!("INSERT INTO canary VALUES (11, '{MARKER}-branch')"))
        .expect("branch insert");
    let branch_note = format!("{MARKER}-branch");
    let on_branch = canary_notes(db);
    assert!(
        on_branch.iter().any(|n| n.contains(&branch_note)),
        "route 10 (branch INSERT) must be readable on the branch that wrote it: {on_branch:?}"
    );
    db.execute("USE BRANCH main").expect("back to main");
    let on_main = canary_notes(db);
    assert!(
        !on_main.iter().any(|n| n.contains(&branch_note)),
        "route 10 wrote to `main`, not to the branch overlay, so the `bdata:` keyspace is not \
         being exercised at all: {on_main:?}"
    );

    // (11) a materialized-view refresh. The base row is inserted AFTER the view
    //      exists, so the refresh has work to do and re-materializes rows
    //      through `insert_tuple` (which also records `delta:` entries).
    db.execute("CREATE MATERIALIZED VIEW canary_mv AS SELECT id, note FROM canary")
        .expect("create materialized view");
    db.execute(&format!("INSERT INTO canary VALUES (10, '{MARKER}-mv-base')"))
        .expect("insert a base row after the view exists");
    db.execute("REFRESH MATERIALIZED VIEW canary_mv")
        .expect("refresh materialized view");
    let mv_rows = render(
        &db.query("SELECT id, note FROM canary_mv", &[])
            .expect("read the materialized view"),
    );
    assert!(
        mv_rows.iter().any(|r| r.contains(&format!("{MARKER}-mv-base"))),
        "route 11 (MV refresh) must materialize the row inserted after the view was created, \
         otherwise the refresh wrote nothing: {mv_rows:?}"
    );

    // (12) RENAME TABLE — a synthesized schema and counter, and every row key
    //      moved, in one batch.
    db.execute("CREATE TABLE canary_rename_src (id INT PRIMARY KEY, note TEXT)")
        .expect("create the table that will be renamed");
    db.execute(&format!("INSERT INTO canary_rename_src VALUES (1, '{MARKER}-renamed')"))
        .expect("seed the table that will be renamed");
    db.execute("ALTER TABLE canary_rename_src RENAME TO canary_rename_dst")
        .expect("rename");
    assert_eq!(
        render(
            &db.query("SELECT id, note FROM canary_rename_dst", &[])
                .expect("read the renamed table")
        ),
        vec![format!("1|{MARKER}-renamed")],
        "route 12 (RENAME TABLE) must carry the row across with the table"
    );

    assert_eq!(
        canary_notes(db),
        expected_canary_notes(),
        "the routes above did not leave `canary` in the state they each just asserted; the \
         on-disk assertions in the caller would be covering a different workload than this \
         function documents"
    );
}

fn canary_notes(db: &EmbeddedDatabase) -> Vec<String> {
    render(
        &db.query("SELECT id, note FROM canary ORDER BY id", &[])
            .expect("select"),
    )
}

// ---------------------------------------------------------------------------
// Encryption-enabled harness (needs a key manager, hence the feature gate)
// ---------------------------------------------------------------------------

#[cfg(feature = "encryption")]
const KEY_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
#[cfg(feature = "encryption")]
const KEY_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// An encrypted configuration over `dir`, keyed from `key_var`.
///
/// A per-test env var NAME is required: `cargo test` runs every test in this
/// binary in ONE process, and environment variables are process-global.
#[cfg(feature = "encryption")]
fn encrypted_config(dir: &Path, key_var: &'static str, key_hex: &str) -> Config {
    std::env::set_var(key_var, key_hex);
    let mut config = plaintext_config(dir);
    config.encryption.enabled = true;
    config.encryption.key_source = heliosdb_nano::KeySource::Environment(key_var.to_string());
    config
}

#[cfg(feature = "encryption")]
fn assert_absent(dir: &Path, needle: &str, what: &str) {
    let hits = files_containing(dir, needle);
    assert!(
        hits.is_empty(),
        "*** ROW DATA IN THE CLEAR *** {what}: the marker {needle:?} was found verbatim in \
         {hits:?} on an encryption-enabled data directory. Some write route reached RocksDB \
         without going through the storage-boundary seal in src/storage/tde.rs."
    );
}

// ---------------------------------------------------------------------------
// 0. The control — ungated, so this binary always runs at least one test
// ---------------------------------------------------------------------------

/// THE CONTROL. With encryption OFF the marker MUST be findable on disk. If it
/// is not, the scan is looking in the wrong place and every ciphertext
/// assertion in this file is vacuous.
#[test]
fn plaintext_control_proves_the_scan_can_see_a_row() {
    let dir = scratch_dir("control");
    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        write_rows_through_every_route(&db);
    }

    let hits = files_containing(&dir, MARKER);
    assert!(
        !hits.is_empty(),
        "control failed: with encryption DISABLED the marker must be visible on disk, otherwise \
         the ciphertext assertions prove nothing. Scanned {} files under {:?}",
        all_files(&dir).len(),
        dir
    );

    // The pre-image must be visible too, or the `v:` assertion below would be
    // vacuous for its own separate reason.
    assert!(
        !files_containing(&dir, OLD_MARKER).is_empty(),
        "control failed: the UPDATE pre-image must be visible on disk with encryption disabled, \
         otherwise the MVCC ciphertext assertion proves nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The DEFAULT configuration is unchanged end to end: every route round-trips
/// and survives a reopen. This is the path the perf gate measures.
#[test]
fn encryption_disabled_is_unchanged() {
    let dir = scratch_dir("disabled");
    let written = {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        write_rows_through_every_route(&db);
        canary_notes(&db)
    };
    assert_eq!(
        written,
        expected_canary_notes(),
        "every route must read back with encryption off"
    );

    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("reopen");
        assert_eq!(
            canary_notes(&db),
            written,
            "rows must survive a reopen unchanged with encryption off"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 1 + 2. THE DECISIVE TESTS — on-disk bytes
// ---------------------------------------------------------------------------

/// ★ The end-to-end statement this whole file exists to make.
///
/// Writes rows through every route in [`write_rows_through_every_route`] on an
/// encryption-enabled database, closes it, and asserts the plaintext marker
/// does not appear anywhere in the data directory. It fails loudly if the
/// cipher is not called on any one of them.
///
/// It also covers the MVCC version copy: `OLD_MARKER` is only ever the
/// PRE-IMAGE of an UPDATE, so it exists solely in the `v:` version chain by the
/// time the database closes. Version history is row data, and a boundary that
/// sealed `data:` while leaving `v:` verbatim would leave a complete copy of
/// every superseded row readable.
#[cfg(feature = "encryption")]
#[test]
fn every_sql_write_route_stores_rows_as_ciphertext() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_ALL_ROUTES_KEY";
    let dir = scratch_dir("all_routes");

    let written = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
        // Readable in the writing session — a seal that broke reads would show
        // up here rather than as a silent difference on disk.
        let written = canary_notes(&db);
        assert_eq!(
            written,
            expected_canary_notes(),
            "every route must read back on an encrypted database"
        );
        written
    };

    // (1) The live row image, across all four write routes.
    assert_absent(&dir, MARKER, "live `data:` rows");
    // (2) The MVCC version chain's copy of the pre-UPDATE row.
    assert_absent(&dir, OLD_MARKER, "the `v:` MVCC version chain");

    // And the sealed bytes decode again in a fresh session.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        assert_eq!(canary_notes(&db), written, "every row must survive a reopen intact");
        assert_eq!(
            db.query("SELECT id FROM canary WHERE id = 4", &[])
                .expect("select")
                .len(),
            1,
            "the transaction-committed row must be readable after a reopen"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `CREATE INDEX` backfill is the first operation that performs a full
/// decrypting scan of `data:`, so it is where a mismatch between the writing
/// route and the reading route surfaces. It must also survive a reopen, which
/// is where the index is rebuilt or restored from its snapshot.
///
/// SCOPE. This test deliberately makes NO on-disk byte assertion. `idxsnap:`
/// (the R4.2 durable ART snapshot, written at clean shutdown) is not one of the
/// keyspaces `src/storage/tde.rs` seals — the sealed set is the one
/// [`SEALED_PREFIXES`] mirrors, each entry listed there with its reason.
/// Asserting the marker's
/// absence here would be asserting a property of a keyspace this boundary does
/// not own, and would fail for a reason that has nothing to do with the row
/// write routes under test. The other tests in this file avoid the question by
/// building no secondary index over a marker-bearing column.
#[cfg(feature = "encryption")]
#[test]
fn index_backfill_reads_rows_written_by_every_route() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_BACKFILL_KEY";
    let dir = scratch_dir("backfill");

    let written = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
        db.execute("CREATE INDEX canary_note_idx ON canary (note)")
            .expect("CREATE INDEX must be able to scan rows written by every route");
        assert_eq!(
            db.query("SELECT id FROM canary WHERE id = 1", &[])
                .expect("select")
                .len(),
            1
        );
        canary_notes(&db)
    };
    assert_eq!(
        written,
        expected_canary_notes(),
        "sanity: every route's row must be present"
    );

    // Reopen: the index is restored (or rebuilt by another decrypting scan),
    // and an indexed lookup must find the row it points at.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        assert_eq!(canary_notes(&db), written, "rows must survive the reopen intact");
        assert_eq!(
            db.query(
                &format!("SELECT id FROM canary WHERE note = '{MARKER}-autocommit'"),
                &[]
            )
            .expect("indexed lookup after reopen")
            .len(),
            1,
            "an indexed lookup on an encrypted database must find the row after a reopen"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2b. ★ THE KEYSPACE WALK — the assertion that survives a route being ADDED
// ---------------------------------------------------------------------------

/// The keyspaces `is_row_value_key` (`src/storage/tde.rs`) seals, mirrored here
/// so the walk below can be driven from a list rather than from a hand-written
/// set of cases.
///
/// KEEP IN SYNC. Adding a prefix to `is_row_value_key` without adding it here
/// leaves the new keyspace unwalked; adding it here without adding it there
/// makes [`no_value_in_a_sealed_keyspace_is_readable_as_plaintext`] fail on
/// values that are verbatim by design. The vacuity guard in that test — every
/// prefix it names must actually be OBSERVED on disk — is what turns a stale
/// entry here into a failure rather than into silence.
///
/// Prefix matching is exact-as-written, which is load-bearing in two places:
/// `wal:entries:` does not match `wal:last_lsn`, and `v:` does not match
/// `v_idx:` / `vmeta:` / `vecsnap:` — each of those is excluded on
/// `is_row_value_key` with its reason.
#[cfg(feature = "encryption")]
const SEALED_PREFIXES: [&str; 6] = ["data:", "v:", "counter:", "wal:entries:", "bdata:", "delta:"];

/// The prefix in [`SEALED_PREFIXES`] that `key` belongs to, if any.
#[cfg(feature = "encryption")]
fn sealed_prefix_of(key: &[u8]) -> Option<&'static str> {
    SEALED_PREFIXES.into_iter().find(|p| key.starts_with(p.as_bytes()))
}

/// A key manager over the same env-var key source a test's `encrypted_config`
/// used, so the walk can check stored bytes against the database's own key.
#[cfg(feature = "encryption")]
fn key_manager_for(key_var: &str) -> heliosdb_nano::crypto::KeyManager {
    heliosdb_nano::crypto::KeyManager::from_source(&heliosdb_nano::KeySource::Environment(key_var.to_string()))
        .expect("the test key must build a key manager")
}

/// THE CONTROL for the walk: it must read VALUES, not just keys.
///
/// With encryption off, at least one value under `data:` holds the marker
/// verbatim. Without this, a walk that visited zero pairs — or that handed back
/// empty values — would make every assertion in
/// [`no_value_in_a_sealed_keyspace_is_readable_as_plaintext`] pass for the wrong
/// reason. Ungated, so it runs whatever the feature set.
#[test]
fn plaintext_control_proves_the_keyspace_walk_reads_values() {
    let dir = scratch_dir("walk_control");
    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        write_rows_through_every_route(&db);
    }

    let mut data_values = 0usize;
    let mut marker_values = 0usize;
    let total = for_each_stored_pair(&dir, |key, value| {
        if key.starts_with(b"data:") {
            data_values += 1;
            if contains_bytes(value, MARKER.as_bytes()) {
                marker_values += 1;
            }
        }
    });

    assert!(total > 0, "the keyspace walk visited no pairs at all under {dir:?}");
    assert!(
        data_values > 0,
        "the keyspace walk visited {total} pairs but none under `data:`, so it is not reading the \
         keyspace the row writers use"
    );
    assert!(
        marker_values > 0,
        "the keyspace walk read {data_values} `data:` values with encryption DISABLED and found \
         the marker in none of them, so it is not reading VALUES"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// ★ THE TEST THAT FAILS WHEN A NEW WRITE ROUTE IS ADDED AND FORGOTTEN.
///
/// Every other on-disk assertion in this file searches for one string that one
/// known route wrote. That answers "did THIS route seal?" and nothing else: a
/// route added next year, writing a row nobody thought to put a marker in,
/// passes all of them.
///
/// This one asks the question the other way round. It walks the ENTIRE stored
/// keyspace of a database that has been driven through
/// [`write_rows_through_every_route`] and makes two claims about every pair it
/// finds:
///
///   1. **Every value under a sealed prefix authenticates under this database's
///      key.** Not "does not look like plaintext" — authenticates. A value that
///      some new writer put there without going through
///      `src/storage/tde.rs::seal*` fails the GCM tag check and is reported by
///      KEY, so the failure names the route's own keyspace. Forging a pass
///      would mean forging a tag: 2^-128.
///   2. **No value OUTSIDE the sealed prefixes contains a row marker.** That is
///      how a brand-new keyspace holding user data announces itself. The
///      exclusions on `is_row_value_key` that DO hold user data verbatim —
///      `cas:` / `dict:` / `col:` — are reached only by non-default column
///      `STORAGE` modes, which this workload does not use; each has its own
///      pinning test further down.
///
/// The vacuity guard is the third assertion, and it is what stops this from
/// degenerating into "a database with nothing in it passes": each prefix the
/// workload is expected to populate must have been OBSERVED. `wal:entries:` is
/// deliberately not in that list — the logical WAL is off in this configuration
/// and its entries are truncated by replay at open, so requiring it would make
/// the guard assert about a keyspace whose contents change underneath it. It is
/// still WALKED (claim 1 applies to whatever is there), and it has its own test
/// in `the_logical_wal_stores_rows_as_ciphertext`.
#[cfg(feature = "encryption")]
#[test]
fn no_value_in_a_sealed_keyspace_is_readable_as_plaintext() {
    use heliosdb_nano::crypto::{self, DecryptAttempt};
    use std::collections::BTreeMap;

    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_KEYSPACE_WALK_KEY";
    let dir = scratch_dir("keyspace_walk");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
    }

    let km = key_manager_for(KEY_VAR);
    let mut seen: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unsealed: Vec<String> = Vec::new();
    let mut leaked: Vec<String> = Vec::new();

    let total = for_each_stored_pair(&dir, |key, value| match sealed_prefix_of(key) {
        Some(prefix) => {
            *seen.entry(prefix).or_insert(0) += 1;
            if !matches!(crypto::try_decrypt(km.key(), value), DecryptAttempt::Authenticated(_)) {
                unsealed.push(format!(
                    "{} ({} bytes)",
                    String::from_utf8_lossy(key).into_owned(),
                    value.len()
                ));
            }
        }
        None => {
            if contains_bytes(value, MARKER.as_bytes()) || contains_bytes(value, OLD_MARKER.as_bytes()) {
                leaked.push(String::from_utf8_lossy(key).into_owned());
            }
        }
    });
    assert!(total > 0, "the keyspace walk visited no pairs at all under {dir:?}");

    // (1) Everything in a sealed keyspace is ciphertext under THIS key.
    assert!(
        unsealed.is_empty(),
        "*** ROW DATA IN THE CLEAR *** {} of {} stored values under the sealed prefixes {:?} do \
         not authenticate under this database's key, so some writer reached RocksDB without going \
         through the storage-boundary seal in src/storage/tde.rs. Offending keys: {:?}",
        unsealed.len(),
        total,
        SEALED_PREFIXES,
        unsealed
    );

    // (2) No keyspace outside the sealed set is carrying a row.
    assert!(
        leaked.is_empty(),
        "*** ROW DATA IN AN UNSEALED KEYSPACE *** these keys are outside every prefix in {:?} and \
         their values hold a row marker verbatim: {:?}. Either the keyspace must be added to \
         `is_row_value_key` in src/storage/tde.rs (and to SEALED_PREFIXES here), or, if it is \
         verbatim by design, it needs a test pinning that decision the way the `cas:` / `dict:` / \
         `col:` tests in this file do.",
        SEALED_PREFIXES,
        leaked
    );

    // (3) The vacuity guard: the walk actually reached each keyspace the
    //     workload populates.
    for prefix in ["data:", "v:", "counter:", "bdata:", "delta:"] {
        assert!(
            seen.get(prefix).copied().unwrap_or(0) > 0,
            "the walk found NO stored value under {prefix:?} after driving every write route, so \
             claim (1) says nothing about that keyspace. Either the route that populates it \
             stopped writing, or it now writes somewhere else. Observed: {seen:?} over {total} \
             stored pairs"
        );
    }

    // The database must still open and read after being walked — an assertion
    // about bytes is worth nothing if those bytes are no longer a database.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        assert_eq!(
            canary_notes(&db),
            expected_canary_notes(),
            "every route's row must still read back after the walk"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. Wrong-key rejection
// ---------------------------------------------------------------------------

/// Reopening an encrypted database under a DIFFERENT key must not hand back
/// rows. Two distinct claims, asserted separately because they can fail
/// independently:
///
///   * the SECURITY claim — the plaintext is not recoverable. This is
///     unconditional: recovering it would mean forging a GCM tag.
///   * the ENGINEERING claim — the read FAILS. It must not come back as
///     garbage values, and it must not come back as a quietly empty result
///     set, either of which a caller would take at face value.
///
/// On why the failure is reliable rather than probabilistic: with the wrong
/// key the AEAD tag check fails, the value is passed through to the row decoder
/// as if it were plaintext, and the decoder reads its leading 8 bytes — the
/// AES-GCM nonce, which is random — as the tuple's value count. For a row to
/// decode "successfully" that random `u64` would have to land within the few
/// dozen values the remaining bytes could supply AND every following byte would
/// have to be a valid `Value` discriminant with a valid payload. `SELECT *`
/// (which decodes the whole tuple rather than a projection) makes that the
/// only route, and its probability is on the order of 2^-64 per row.
#[cfg(feature = "encryption")]
#[test]
fn reopening_with_a_different_key_does_not_expose_rows() {
    const KEY_VAR_RIGHT: &str = "HELIOSDB_TEST_AT_REST_RIGHT_KEY";
    const KEY_VAR_WRONG: &str = "HELIOSDB_TEST_AT_REST_WRONG_KEY";
    let dir = scratch_dir("wrong_key");

    let truth = {
        let db =
            EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_RIGHT, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
        canary_notes(&db)
    };
    assert_eq!(
        truth,
        expected_canary_notes(),
        "sanity: the correct key must read every route's row"
    );

    // Sanity: the SAME configuration reopens fine. Without this, an assertion
    // that "the wrong key fails" would also pass if the database simply could
    // never be reopened at all.
    {
        let db =
            EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_RIGHT, KEY_A)).expect("reopen, right key");
        assert_eq!(canary_notes(&db), truth, "sanity: the right key must still work");
    }

    // Now the different key. Opening may fail outright or may succeed and fail
    // at the read — both are correct, and which one happens is an internal
    // detail (unsealed catalog metadata still loads). What must NOT happen is
    // a successful read.
    let outcome: heliosdb_nano::Result<Vec<String>> =
        match EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_WRONG, KEY_B)) {
            Err(e) => Err(e),
            Ok(db) => db
                .query("SELECT * FROM canary ORDER BY id", &[])
                .map(|rows| render(&rows)),
        };

    match outcome {
        Err(_) => {
            // Correct: the wrong key does not open the data.
        }
        Ok(rows) => {
            let joined = rows.join(" ");
            assert!(
                !joined.contains(MARKER) && !joined.contains(OLD_MARKER),
                "*** WRONG KEY READ THE PLAINTEXT *** a session holding a different key recovered \
                 row contents: {rows:?}"
            );
            panic!(
                "a read under the wrong key must FAIL, not return {} row(s) of garbage or an \
                 empty result a caller would trust: {rows:?}",
                rows.len()
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. Mixed-format tolerance
// ---------------------------------------------------------------------------

/// An existing field database: rows written before the storage boundary sealed
/// anything sit under the SAME `data:` prefix as rows written after. Stored
/// values are untagged, so nothing on disk distinguishes the two, and a reader
/// that assumed "key manager present ⇒ every value is ciphertext" could not
/// read data that is already deployed.
///
/// The seam used here is the honest one — the public API. A first session
/// writes with encryption DISABLED; a second session opens the SAME directory
/// with encryption ENABLED and writes more rows. That leaves genuine plaintext
/// and genuine ciphertext side by side under one prefix, exactly as an upgrade
/// in the field would.
///
/// Two things are asserted, and the second is what makes this more than a
/// tolerance test: on that single directory, the OLD marker is still on disk in
/// the clear (it was written that way and nothing rewrites it) while the NEW
/// marker is not (post-upgrade writes are sealed).
#[cfg(feature = "encryption")]
#[test]
fn a_database_with_both_plaintext_and_ciphertext_rows_reads_correctly() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_MIXED_KEY";
    let dir = scratch_dir("mixed");

    // Session 1 — an UNENCRYPTED database, as it exists in the field today.
    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        db.execute("CREATE TABLE canary (id INT PRIMARY KEY, note TEXT)")
            .expect("create table");
        db.execute(&format!("INSERT INTO canary VALUES (1, '{LEGACY_MARKER}-one')"))
            .expect("legacy insert");
        db.execute(&format!("INSERT INTO canary VALUES (2, '{LEGACY_MARKER}-two')"))
            .expect("legacy insert");
    }

    // Session 2 — the same directory, now opened WITH encryption. The legacy
    // rows must still read, and new rows must be sealed.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("upgrade to encrypted");

        let legacy = render(
            &db.query("SELECT id, note FROM canary ORDER BY id", &[])
                .expect("legacy rows must still be readable after enabling encryption"),
        );
        assert_eq!(
            legacy,
            vec![format!("1|{LEGACY_MARKER}-one"), format!("2|{LEGACY_MARKER}-two"),],
            "a mixed-format database must read its pre-existing plaintext rows unchanged"
        );

        db.execute(&format!("INSERT INTO canary VALUES (3, '{MARKER}-post-upgrade')"))
            .expect("post-upgrade insert");
        db.execute(&format!(
            "INSERT INTO canary VALUES (4, '{MARKER}-post-upgrade-b'), (5, '{MARKER}-post-upgrade-c')"
        ))
        .expect("post-upgrade multi-row insert");

        // Both formats, one prefix, one result set, in the right order.
        let mixed = render(
            &db.query("SELECT id, note FROM canary ORDER BY id", &[])
                .expect("mixed plaintext/ciphertext scan"),
        );
        assert_eq!(
            mixed,
            vec![
                format!("1|{LEGACY_MARKER}-one"),
                format!("2|{LEGACY_MARKER}-two"),
                format!("3|{MARKER}-post-upgrade"),
                format!("4|{MARKER}-post-upgrade-b"),
                format!("5|{MARKER}-post-upgrade-c"),
            ],
            "a scan spanning both formats must return every row correctly"
        );

        // A predicate over the mixed set, so the tolerant decode is exercised
        // by a filtered/projected scan as well as a full one.
        let filtered = render(
            &db.query("SELECT note FROM canary WHERE id = 2", &[])
                .expect("filtered read of a legacy row"),
        );
        assert_eq!(filtered, vec![format!("{LEGACY_MARKER}-two")]);

        // An UPDATE of a LEGACY row: the pre-image copied into the `v:` chain
        // is a plaintext value being read by an encryption-enabled writer, the
        // exact place a "ciphertext only" assumption would corrupt data.
        db.execute("UPDATE canary SET note = 'rewritten' WHERE id = 1")
            .expect("update of a legacy plaintext row");
        let after = render(
            &db.query("SELECT note FROM canary WHERE id = 1", &[])
                .expect("read back"),
        );
        assert_eq!(after, vec!["rewritten".to_string()]);
    }

    // The tolerant path was really taken — this counter only moves when an
    // encryption-ENABLED reader accepts a stored value as plaintext.
    //
    // The counter is process-global and monotonic, and this binary's tests run
    // in parallel, so only a lower bound is meaningful. `>= 1` still has teeth:
    // it is 0 unless some encryption-enabled reader actually took the
    // passthrough branch.
    assert!(
        heliosdb_nano::StorageEngine::plaintext_passthrough_count() >= 1,
        "the mixed-format read did not exercise the plaintext passthrough at all, so this test \
         is not proving what it claims"
    );

    // On ONE directory: the pre-upgrade rows are on disk in the clear (they
    // were written that way, and enabling encryption does not rewrite history),
    // while everything written after the upgrade is sealed.
    assert!(
        !files_containing(&dir, LEGACY_MARKER).is_empty(),
        "the mixed-format simulation is not a mixture: the pre-upgrade plaintext rows are absent \
         from disk, so this test never had a plaintext value to tolerate"
    );
    assert_absent(&dir, MARKER, "rows written after encryption was enabled");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 5. SQL behaviour parity
// ---------------------------------------------------------------------------

/// Run one identical DML/query script and record everything observable:
/// affected-row counts from each statement and every result set.
#[cfg(feature = "encryption")]
fn dml_script_observations(db: &EmbeddedDatabase) -> Vec<String> {
    let mut obs = Vec::new();
    let mut note = |label: &str, value: String| obs.push(format!("{label}={value}"));

    db.execute("CREATE TABLE parity (id INT PRIMARY KEY, name TEXT, qty INT)")
        .expect("create table");

    note(
        "insert_one",
        db.execute("INSERT INTO parity VALUES (1, 'alpha', 10)")
            .expect("insert one")
            .to_string(),
    );
    note(
        "insert_many",
        db.execute("INSERT INTO parity VALUES (2, 'beta', 20), (3, 'gamma', 30)")
            .expect("insert many")
            .to_string(),
    );
    {
        let tx = db.begin_transaction().expect("begin");
        note(
            "insert_txn",
            tx.execute("INSERT INTO parity VALUES (4, 'delta', 40)")
                .expect("txn insert")
                .to_string(),
        );
        tx.commit().expect("commit");
    }
    note(
        "select_all_after_insert",
        render(
            &db.query("SELECT id, name, qty FROM parity ORDER BY id", &[])
                .expect("select all"),
        )
        .join(","),
    );
    note(
        "update",
        db.execute("UPDATE parity SET qty = 99 WHERE id = 2")
            .expect("update")
            .to_string(),
    );
    note(
        "select_after_update",
        render(
            &db.query("SELECT id, name, qty FROM parity ORDER BY id", &[])
                .expect("select after update"),
        )
        .join(","),
    );
    note(
        "delete",
        db.execute("DELETE FROM parity WHERE id = 3")
            .expect("delete")
            .to_string(),
    );
    note(
        "select_after_delete",
        render(
            &db.query("SELECT id, name, qty FROM parity ORDER BY id", &[])
                .expect("select after delete"),
        )
        .join(","),
    );
    note(
        "count",
        render(&db.query("SELECT COUNT(*) FROM parity", &[]).expect("count")).join(","),
    );
    note(
        "filtered",
        render(
            &db.query("SELECT name FROM parity WHERE qty >= 40 ORDER BY name", &[])
                .expect("filtered"),
        )
        .join(","),
    );
    note(
        "aggregate",
        render(&db.query("SELECT SUM(qty) FROM parity", &[]).expect("aggregate")).join(","),
    );

    obs
}

/// INSERT / SELECT / UPDATE / DELETE must behave IDENTICALLY with encryption on
/// and off — same affected-row counts, same rows, same order, same aggregates.
/// Encryption is transparent or it is not encryption at rest; it is a storage
/// property and must be invisible above the storage boundary.
#[cfg(feature = "encryption")]
#[test]
fn sql_round_trip_is_identical_with_and_without_encryption() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_PARITY_KEY";
    let plain_dir = scratch_dir("parity_plain");
    let enc_dir = scratch_dir("parity_enc");

    let plain = {
        let db = EmbeddedDatabase::with_config(plaintext_config(&plain_dir)).expect("plain database");
        dml_script_observations(&db)
    };
    let encrypted = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&enc_dir, KEY_VAR, KEY_A)).expect("encrypted database");
        dml_script_observations(&db)
    };

    // A vacuity guard: if the script ever degenerated to nothing, comparing two
    // empty vectors would pass and mean nothing.
    assert!(
        plain.len() >= 11 && plain.iter().any(|o| o.contains("alpha")),
        "the parity script recorded nothing meaningful: {plain:?}"
    );
    assert_eq!(
        encrypted, plain,
        "encryption changed observable SQL behaviour; it must be transparent above the storage \
         boundary"
    );

    // And identical again after a reopen from disk on both sides.
    let plain_reopen = {
        let db = EmbeddedDatabase::with_config(plaintext_config(&plain_dir)).expect("reopen plain");
        render(
            &db.query("SELECT id, name, qty FROM parity ORDER BY id", &[])
                .expect("reopen select"),
        )
    };
    let encrypted_reopen = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&enc_dir, KEY_VAR, KEY_A)).expect("reopen encrypted");
        render(
            &db.query("SELECT id, name, qty FROM parity ORDER BY id", &[])
                .expect("reopen select"),
        )
    };
    assert_eq!(
        plain_reopen.len(),
        3,
        "sanity: three rows must remain after the script: {plain_reopen:?}"
    );
    assert_eq!(
        encrypted_reopen, plain_reopen,
        "an encrypted database must return the same rows as a plain one after a reopen"
    );

    let _ = std::fs::remove_dir_all(&plain_dir);
    let _ = std::fs::remove_dir_all(&enc_dir);
}

// ---------------------------------------------------------------------------
// 6. The logical WAL — a second full copy of every row it logs
// ---------------------------------------------------------------------------
//
// `wal:entries:{lsn}` values are `WalEntry`s whose `Insert`/`Update` operations
// carry the row tuple itself, written into the SAME RocksDB store as the
// `data:` key they describe. `storage.wal_enabled` is TRUE by default
// (`src/config.rs`), so this keyspace is part of the shipped configuration, and
// it is a row keyspace: `src/storage/tde.rs` lists it as sealed and
// `WriteAheadLog` applies the rule at its own boundary.
//
// The routes that log a full row on the DEFAULT configuration (that is, with
// `logical_wal_per_statement` left at its default `false`) are the transaction
// COMMIT batch and autocommit UPDATE/DELETE — routes (3) and (4) of
// `write_rows_through_every_route`, both of which carry MARKER.

/// The row images carried by this database's logical WAL entries.
///
/// Reads through `StorageEngine::get`, which applies the storage boundary's
/// decode, so this returns the in-memory form whatever the stored form is —
/// which is the point: it establishes that the entries EXIST and carry the
/// row, so the on-disk assertion below cannot pass vacuously against a WAL that
/// simply logged nothing.
fn wal_row_images(db: &EmbeddedDatabase) -> Vec<String> {
    use heliosdb_nano::storage::{WalEntry, WalOperation};

    let Some(bytes) = db.storage.get(&b"wal:last_lsn".to_vec()).expect("read wal:last_lsn") else {
        return Vec::new();
    };
    let last_lsn = u64::from_le_bytes(bytes.as_slice().try_into().expect("wal:last_lsn is 8 bytes"));

    let mut images = Vec::new();
    for lsn in 1..=last_lsn {
        let key = format!("wal:entries:{lsn:020}").into_bytes();
        let Some(raw) = db.storage.get(&key).expect("read a WAL entry") else {
            continue;
        };
        let entry = WalEntry::deserialize(&raw).expect("a stored WAL entry must decode at the storage boundary");
        match entry.operation {
            WalOperation::Insert { tuple, .. } | WalOperation::Update { tuple, .. } => {
                images.push(String::from_utf8_lossy(&tuple).into_owned());
            }
            _ => {}
        }
    }
    images
}

/// The shipped default: the logical WAL ON.
fn wal_config(dir: &Path) -> Config {
    let mut config = plaintext_config(dir);
    config.storage.wal_enabled = true;
    config
}

/// THE CONTROL for the WAL assertion. With encryption OFF the row logged into
/// `wal:entries:` must be findable on disk — otherwise the ciphertext assertion
/// below would prove nothing.
#[test]
fn wal_plaintext_control_proves_the_wal_carries_a_row() {
    let dir = scratch_dir("wal_control");
    let images = {
        let db = EmbeddedDatabase::with_config(wal_config(&dir)).expect("plain database");
        write_rows_through_every_route(&db);
        wal_row_images(&db)
    };

    assert!(
        images.iter().any(|image| image.contains(MARKER)),
        "control failed: the logical WAL logged no row carrying the marker, so the ciphertext \
         assertion in the encrypted case would be vacuous. Images: {images:?}"
    );
    assert!(
        !files_containing(&dir, MARKER).is_empty(),
        "control failed: with encryption DISABLED the row must be visible on disk"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The logical WAL's row images are sealed like every other row keyspace, and a
/// WAL written that way still replays: the reopen below runs
/// `StorageEngine::replay_wal` over exactly these entries.
#[cfg(feature = "encryption")]
#[test]
fn the_logical_wal_stores_rows_as_ciphertext() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_WAL_KEY";
    let dir = scratch_dir("wal_encrypted");

    let (images, rows) = {
        let mut config = encrypted_config(&dir, KEY_VAR, KEY_A);
        config.storage.wal_enabled = true;
        let db = EmbeddedDatabase::with_config(config).expect("encrypted database");
        write_rows_through_every_route(&db);
        (wal_row_images(&db), canary_notes(&db))
    };

    // Vacuity guard: the same check the control makes. Without it, "the marker
    // is not on disk" would also hold for a WAL that logged nothing at all.
    assert!(
        images.iter().any(|image| image.contains(MARKER)),
        "the logical WAL logged no row on the encrypted database either, so nothing below is being \
         tested. Images: {images:?}"
    );

    assert_absent(&dir, MARKER, "the logical WAL's row images");
    assert_absent(&dir, OLD_MARKER, "the logical WAL's UPDATE pre-image entries");

    // Reopen: `replay_wal` reads every one of those entries back. If the WAL
    // codec disagreed with itself in either direction, this is where it shows.
    {
        let mut config = encrypted_config(&dir, KEY_VAR, KEY_A);
        config.storage.wal_enabled = true;
        let db = EmbeddedDatabase::with_config(config).expect("reopen must replay the sealed WAL");
        assert_eq!(
            canary_notes(&db),
            rows,
            "the rows must be unchanged after a reopen that replayed the logical WAL"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 7. MERGE BRANCH — a row moving between keyspaces
// ---------------------------------------------------------------------------

/// `MERGE BRANCH x INTO main` moves a row from the branch overlay keyspace into
/// `data:`, which is a sealed keyspace. The stored form of a value is a property
/// of the key it lands under, so the merge decodes for the source key and seals
/// for the destination key.
///
/// SCOPE, STATED RATHER THAN IMPLIED: what this test asserts is the destination
/// side — the merged row is readable on main through the `data:`-keyspace
/// readers (before and after a reopen), and a value written over it on main
/// afterwards is sealed. The branch overlay keyspace's own bytes are the subject
/// of `rows_written_on_a_branch_are_sealed_in_the_overlay_keyspace` below; a
/// byte scan here could not tell the two copies apart.
#[cfg(feature = "encryption")]
#[test]
fn merging_a_branch_into_main_writes_the_merged_row_in_the_form_main_expects() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_MERGE_KEY";
    const BRANCH_MARKER: &str = "QZX-TDE-BRANCH-ROW-6612";
    const MERGED_MARKER: &str = "QZX-TDE-MERGED-ROW-7734";
    let dir = scratch_dir("merge_branch");

    let after_merge = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute("CREATE TABLE canary (id INT PRIMARY KEY, note TEXT)")
            .expect("create table");
        db.execute(&format!("INSERT INTO canary VALUES (1, '{MERGED_MARKER}-main')"))
            .expect("main insert");

        db.execute("CREATE BRANCH dev AS OF NOW").expect("create branch");
        db.execute("USE BRANCH dev").expect("use branch");
        db.execute(&format!("INSERT INTO canary VALUES (2, '{BRANCH_MARKER}')"))
            .expect("branch insert");
        db.execute("USE BRANCH main").expect("back to main");
        db.execute("MERGE BRANCH dev INTO main").expect("merge");

        let notes = canary_notes(&db);
        assert_eq!(
            notes.len(),
            2,
            "sanity: the merge must move the branch row onto main, else nothing below is tested: {notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains(BRANCH_MARKER)),
            "the merged row must be READABLE on main — a value stored in a form main's readers do \
             not expect would surface here: {notes:?}"
        );

        // Write over the merged row on main. This reads the merged `data:`
        // value (so it must decode) and re-seals it under the same key.
        db.execute(&format!(
            "UPDATE canary SET note = '{MERGED_MARKER}-rewritten' WHERE id = 2"
        ))
        .expect("update the merged row");
        canary_notes(&db)
    };

    assert_absent(&dir, MERGED_MARKER, "rows written on main around a MERGE BRANCH");

    // And it survives a reopen, i.e. the merged row was durable in a form the
    // `data:` readers accept, not merely correct in a warm cache.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        assert_eq!(canary_notes(&db), after_merge, "the merged rows must survive a reopen");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Drive branch DML: a row INSERTed on a non-`main` branch and a row UPDATEd
/// there. Both land in the `bdata:` overlay keyspace.
fn write_rows_on_a_branch(db: &EmbeddedDatabase, insert_marker: &str, update_marker: &str) {
    db.execute("CREATE TABLE canary (id INT PRIMARY KEY, note TEXT)")
        .expect("create table");
    db.execute("INSERT INTO canary VALUES (1, 'main row')")
        .expect("main insert");

    db.execute("CREATE BRANCH dev AS OF NOW").expect("create branch");
    db.execute("USE BRANCH dev").expect("use branch");
    // (1) branch INSERT — reaches `bdata:` through the commit `WriteBatch`.
    db.execute(&format!("INSERT INTO canary VALUES (2, '{insert_marker}')"))
        .expect("branch insert");
    // (2) branch UPDATE of a row that already lives in the overlay — reaches
    //     `bdata:` through `StorageEngine::put`.
    db.execute("INSERT INTO canary VALUES (3, 'to be updated on the branch')")
        .expect("branch insert for update");
    db.execute(&format!("UPDATE canary SET note = '{update_marker}' WHERE id = 3"))
        .expect("branch update");

    let on_branch = canary_notes(db);
    assert!(
        on_branch.iter().any(|n| n.contains(insert_marker)) && on_branch.iter().any(|n| n.contains(update_marker)),
        "sanity: the branch rows must be readable on the branch, else nothing is being tested: {on_branch:?}"
    );
    db.execute("USE BRANCH main").expect("back to main");
}

/// THE CONTROL for the branch-overlay assertion below: with encryption OFF the
/// branch rows MUST be findable on disk, or the scan is not reaching them and
/// the ciphertext assertion is vacuous.
#[test]
fn plaintext_control_proves_the_scan_can_see_a_branch_row() {
    const INSERTED: &str = "QZX-TDE-BRANCH-INSERT-CONTROL-3310";
    const UPDATED: &str = "QZX-TDE-BRANCH-UPDATE-CONTROL-3311";
    let dir = scratch_dir("branch_control");
    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        write_rows_on_a_branch(&db, INSERTED, UPDATED);
    }

    for marker in [INSERTED, UPDATED] {
        assert!(
            !files_containing(&dir, marker).is_empty(),
            "the branch-overlay scan found nothing for {marker:?} with encryption OFF, so the \
             encrypted assertion would pass for the wrong reason"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A row written on a branch is a full user row image, so the `bdata:` overlay
/// is sealed like `data:` is. Both branch write routes are exercised: the branch
/// INSERT (which reaches `bdata:` through the commit `WriteBatch`) and the branch
/// UPDATE (which reaches it through `StorageEngine::put`).
#[cfg(feature = "encryption")]
#[test]
fn rows_written_on_a_branch_are_sealed_in_the_overlay_keyspace() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_BRANCH_KEY";
    const INSERTED: &str = "QZX-TDE-BRANCH-INSERT-8841";
    const UPDATED: &str = "QZX-TDE-BRANCH-UPDATE-8842";
    let dir = scratch_dir("branch_overlay");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        write_rows_on_a_branch(&db, INSERTED, UPDATED);
    }

    assert_absent(&dir, INSERTED, "a row INSERTed on a branch");
    assert_absent(&dir, UPDATED, "a row UPDATEd on a branch");

    // …and the sealed overlay still reads back after a reopen, so this is
    // sealing rather than losing.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        db.execute("USE BRANCH dev").expect("use branch");
        let on_branch = canary_notes(&db);
        assert!(
            on_branch.iter().any(|n| n.contains(INSERTED)) && on_branch.iter().any(|n| n.contains(UPDATED)),
            "the sealed branch rows must still be readable after a reopen: {on_branch:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 7b. DELETE — the row that is no longer a row
// ---------------------------------------------------------------------------

/// Delete rows through both DELETE shapes: a PK-equality predicate and a
/// non-PK-column predicate (which takes the general
/// `delete_tuples_branch_aware` funnel).
fn delete_rows_through_both_predicates(db: &EmbeddedDatabase, pk_marker: &str, scan_marker: &str) {
    db.execute("CREATE TABLE canary (id INT PRIMARY KEY, note TEXT)")
        .expect("create table");
    db.execute(&format!("INSERT INTO canary VALUES (1, '{pk_marker}')"))
        .expect("insert");
    db.execute(&format!("INSERT INTO canary VALUES (2, '{scan_marker}')"))
        .expect("insert");

    db.execute("DELETE FROM canary WHERE id = 1").expect("pk delete");
    db.execute(&format!("DELETE FROM canary WHERE note = '{scan_marker}'"))
        .expect("predicate delete");

    assert!(
        canary_notes(db).is_empty(),
        "sanity: both rows must actually be gone, else nothing is being tested"
    );
}

/// THE CONTROL: with encryption OFF a deleted row's contents ARE still on disk
/// (in the MVCC pre-image and the MV delta log), so the scan below is looking in
/// the right place.
#[test]
fn plaintext_control_proves_the_scan_can_see_a_deleted_row() {
    const PK: &str = "QZX-TDE-DELETED-PK-CONTROL-7720";
    const SCAN: &str = "QZX-TDE-DELETED-SCAN-CONTROL-7721";
    let dir = scratch_dir("delete_control");
    {
        let db = EmbeddedDatabase::with_config(plaintext_config(&dir)).expect("plain database");
        delete_rows_through_both_predicates(&db, PK, SCAN);
    }

    for marker in [PK, SCAN] {
        assert!(
            !files_containing(&dir, marker).is_empty(),
            "the scan found nothing for the deleted row {marker:?} with encryption OFF, so the \
             encrypted assertion would pass for the wrong reason"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A DELETE keeps copies of the row it removed — the MVCC pre-image in `v:` and
/// a `delta:` record carrying the whole `Tuple` for incremental MV refresh, which
/// nothing compacts. Neither may hold the row in the clear on an encrypted
/// database: a DELETE that left a readable copy of the row beside the ciphertext
/// it deleted it from would be the one write route where deleting data published
/// it.
#[cfg(feature = "encryption")]
#[test]
fn a_deleted_rows_contents_are_not_left_in_the_clear() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_DELETE_KEY";
    const PK: &str = "QZX-TDE-DELETED-PK-7730";
    const SCAN: &str = "QZX-TDE-DELETED-SCAN-7731";
    let dir = scratch_dir("delete_sealed");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        delete_rows_through_both_predicates(&db, PK, SCAN);
    }

    assert_absent(&dir, PK, "a row deleted by PK equality");
    assert_absent(&dir, SCAN, "a row deleted by a non-PK predicate");

    // The database must still open and read afterwards — the assertion above
    // would also hold for a directory this build could no longer make sense of.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        assert!(canary_notes(&db).is_empty(), "the deleted rows must stay deleted");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 8. Key verification at open
// ---------------------------------------------------------------------------
//
// The tolerant read cannot distinguish "this stored value predates sealing"
// from "this value was sealed under a different key" — both are an AEAD tag
// failure and nothing on disk separates them. The key-check sentinel makes that
// distinction once, at open, with the strict decrypt, which is what keeps
// per-value tolerance meaning only what it says.

/// A wrong key must be refused AT OPEN, by name, rather than turning every
/// stored value into an unauthenticated buffer that the row decoders then treat
/// as if it were plaintext.
#[cfg(feature = "encryption")]
#[test]
fn a_wrong_key_is_refused_at_open_with_a_message_that_names_the_cause() {
    const KEY_VAR_RIGHT: &str = "HELIOSDB_TEST_AT_REST_SENTINEL_RIGHT";
    const KEY_VAR_WRONG: &str = "HELIOSDB_TEST_AT_REST_SENTINEL_WRONG";
    let dir = scratch_dir("sentinel");

    {
        let db =
            EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_RIGHT, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
    }

    let err = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_WRONG, KEY_B))
        .err()
        .expect("opening with a key this database was not sealed with must FAIL, not read garbage");
    let msg = err.to_string();
    assert!(
        msg.contains("does not match this database"),
        "the failure must say the key is wrong, not surface as a decode error somewhere later: {msg}"
    );

    // The right key still opens it — without this, the assertion above would
    // also pass if the database had simply become unopenable.
    {
        let db =
            EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR_RIGHT, KEY_A)).expect("reopen, right key");
        assert_eq!(
            canary_notes(&db),
            expected_canary_notes(),
            "the right key must still read every row"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The mirror case: an encrypted database opened with encryption switched OFF.
/// Its stored values are ciphertext and no key is configured to open them, so
/// this must also fail at open rather than hand ciphertext to the row decoders.
#[cfg(feature = "encryption")]
#[test]
fn an_encrypted_database_is_refused_when_encryption_is_switched_off() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_SENTINEL_OFF";
    let dir = scratch_dir("sentinel_off");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        write_rows_through_every_route(&db);
    }

    let err = EmbeddedDatabase::with_config(plaintext_config(&dir))
        .err()
        .expect("an encrypted database must not open with encryption disabled");
    assert!(
        err.to_string().contains("written with encryption enabled"),
        "the failure must name the missing configuration: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 9. Non-default column STORAGE modes — the boundary of what this seal covers
// ---------------------------------------------------------------------------

/// ★ A PIN, NOT AN ENDORSEMENT.
///
/// This test asserts what the code does TODAY for a column declared
/// `STORAGE CONTENT_ADDRESSED`, so that the limit is executable rather than an
/// assumption. `src/storage/tde.rs` states the same thing in prose in
/// `is_row_value_key`'s exclusion list; this is the part that fails if the prose
/// stops being true in either direction.
///
/// WHAT IS PINNED. For a non-default column storage mode the `data:` row image
/// holds only a REFERENCE — a `Value::CasRef` here — and the payload itself
/// lives in the `cas:{blake3}` sidecar, which `ContentAddressedStore` writes
/// straight to RocksDB with no key manager. Sealing `data:` therefore seals the
/// reference, not the payload. The threshold is `CAS_MIN_SIZE` (1 KiB): a
/// smaller value stays inline in the row and IS sealed with it.
///
/// WHY THE SAME TABLE CARRIES A SECOND, DEFAULT-STORAGE COLUMN. Without it,
/// "the sidecar marker is on disk" would be indistinguishable from "this table's
/// rows are not encrypted at all". The inline column carries an equally long
/// value through the same INSERT, on the same row, and MUST be absent — that is
/// what makes the sidecar's presence a statement about the sidecar.
///
/// IF THIS TEST FAILS, read which assertion failed:
///   * the INLINE assertion — the row itself stopped being sealed; that is a
///     regression in the storage boundary and is the serious case.
///   * the SIDECAR assertion — either the sidecars are now sealed (good: update
///     `is_row_value_key`'s exclusion list and turn this assertion around), or
///     the value no longer takes the content-addressed path at all (check
///     `CAS_MIN_SIZE` and the payload length below).
#[cfg(feature = "encryption")]
#[test]
fn a_content_addressed_column_stores_its_payload_outside_the_seal() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_CAS_KEY";
    const SIDECAR_MARKER: &str = "QZX-TDE-CAS-SIDECAR-5518";
    const INLINE_MARKER: &str = "QZX-TDE-CAS-INLINE-5519";
    let dir = scratch_dir("cas_sidecar");

    // Comfortably over `content_addr::CAS_MIN_SIZE` (1024), so
    // `ContentAddressedStore::maybe_store` takes the sidecar branch.
    let sidecar_value = format!("{SIDECAR_MARKER}{}", "x".repeat(1200));
    // Same length, DEFAULT storage: it stays inline in the row image.
    let inline_value = format!("{INLINE_MARKER}{}", "y".repeat(1200));

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT STORAGE CONTENT_ADDRESSED, note TEXT)")
            .expect("create table with a content-addressed column");
        db.execute(&format!(
            "INSERT INTO docs VALUES (1, '{sidecar_value}', '{inline_value}')"
        ))
        .expect("insert");

        // Both columns must READ back intact — a pin on broken storage would be
        // worthless.
        let rows = db
            .query("SELECT body, note FROM docs WHERE id = 1", &[])
            .expect("select");
        let rendered = render(&rows);
        let expected = vec![format!("{sidecar_value}|{inline_value}")];
        assert_eq!(
            rendered, expected,
            "both columns must resolve to the values that were inserted"
        );
    }

    // The row image itself IS sealed — this is the discriminator that makes the
    // assertion below mean something about the SIDECAR rather than about the
    // table.
    assert_absent(
        &dir,
        INLINE_MARKER,
        "a DEFAULT-storage column on a table that also uses a sidecar",
    );

    // TODAY'S BEHAVIOUR, PINNED: the content-addressed payload is on disk in the
    // clear, because `cas:` is outside the sealed keyspaces.
    let hits = files_containing(&dir, SIDECAR_MARKER);
    assert!(
        !hits.is_empty(),
        "this test pins a KNOWN, DOCUMENTED gap and it just changed. The content-addressed \
         payload for `STORAGE CONTENT_ADDRESSED` was NOT found on disk. If the `cas:` sidecar is \
         now sealed, that is an improvement: update `is_row_value_key`'s exclusion list in \
         src/storage/tde.rs and invert this assertion. If instead the value simply did not take \
         the content-addressed path, check `content_addr::CAS_MIN_SIZE` against the {} bytes \
         inserted here.",
        sidecar_value.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// ★ A PIN, NOT AN ENDORSEMENT — the `dict:` sidecar.
///
/// The sibling of the test above for `STORAGE DICTIONARY`. Every DISTINCT value
/// of such a column is written verbatim into the serialized `ColumnDictionary`
/// at `dict:{table}:{column}` by `dictionary.rs`, which owns no key manager, and
/// the row image holds a `Value::DictRef` code. `is_row_value_key`'s exclusion
/// list in `src/storage/tde.rs` states this; this test is what fails if that
/// stops being true in either direction.
///
/// The second, DEFAULT-storage column is the discriminator, exactly as in the
/// `cas:` test: without it, "the dictionary marker is on disk" would be
/// indistinguishable from "this table's rows are not sealed at all".
#[cfg(feature = "encryption")]
#[test]
fn a_dictionary_column_stores_its_values_outside_the_seal() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_DICT_KEY";
    const DICT_MARKER: &str = "QZX-TDE-DICT-SIDECAR-5620";
    const INLINE_MARKER: &str = "QZX-TDE-DICT-INLINE-5621";
    let dir = scratch_dir("dict_sidecar");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute("CREATE TABLE tagged (id INT PRIMARY KEY, status TEXT STORAGE DICTIONARY, note TEXT)")
            .expect("create table with a dictionary column");
        // Two rows sharing one dictionary entry — the low-cardinality shape the
        // mode exists for, so the value is genuinely stored in the dictionary
        // rather than inline.
        db.execute(&format!(
            "INSERT INTO tagged VALUES (1, '{DICT_MARKER}', '{INLINE_MARKER}-a')"
        ))
        .expect("insert");
        db.execute(&format!(
            "INSERT INTO tagged VALUES (2, '{DICT_MARKER}', '{INLINE_MARKER}-b')"
        ))
        .expect("insert");

        // Both columns must READ back intact — a pin on broken storage would be
        // worthless.
        assert_eq!(
            render(
                &db.query("SELECT id, status, note FROM tagged ORDER BY id", &[])
                    .expect("select")
            ),
            vec![
                format!("1|{DICT_MARKER}|{INLINE_MARKER}-a"),
                format!("2|{DICT_MARKER}|{INLINE_MARKER}-b"),
            ],
            "both columns must resolve to the values that were inserted"
        );
    }

    // The row image itself IS sealed — the discriminator.
    assert_absent(
        &dir,
        INLINE_MARKER,
        "a DEFAULT-storage column on a table that also uses a dictionary column",
    );

    // TODAY'S BEHAVIOUR, PINNED: the dictionary payload is on disk in the clear.
    assert!(
        !files_containing(&dir, DICT_MARKER).is_empty(),
        "this test pins a KNOWN, DOCUMENTED gap and it just changed. The value of a \
         `STORAGE DICTIONARY` column was NOT found on disk. If the `dict:` sidecar is now sealed, \
         that is an improvement: update `is_row_value_key`'s exclusion list in src/storage/tde.rs, \
         add the prefix to SEALED_PREFIXES here, and invert this assertion. If instead the value \
         no longer takes the dictionary path at all, this table's column is not being encoded as \
         a `Value::DictRef` any more."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// ★ A PIN, NOT AN ENDORSEMENT — the `col:` sidecar.
///
/// The sibling of the two tests above for `STORAGE COLUMNAR`. The column's
/// values are written verbatim into the `col:{table}:{column}:{batch}` batches
/// by `columnar.rs`, which owns no key manager, and the row image holds
/// `Value::ColumnarRef`. (`colz:` / `colp:` are the zone-map and presence
/// sidecars beside them; they hold statistics and a bitmap, not values.)
/// `is_row_value_key`'s exclusion list in `src/storage/tde.rs` states this; this
/// test is what fails if that stops being true in either direction.
#[cfg(feature = "encryption")]
#[test]
fn a_columnar_column_stores_its_values_outside_the_seal() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_COLUMNAR_KEY";
    const COLUMN_MARKER: &str = "QZX-TDE-COLUMNAR-SIDECAR-5730";
    const INLINE_MARKER: &str = "QZX-TDE-COLUMNAR-INLINE-5731";
    let dir = scratch_dir("columnar_sidecar");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, tag TEXT STORAGE COLUMNAR, note TEXT)")
            .expect("create table with a columnar column");
        db.execute(&format!(
            "INSERT INTO metrics VALUES (1, '{COLUMN_MARKER}', '{INLINE_MARKER}')"
        ))
        .expect("insert");

        assert_eq!(
            render(&db.query("SELECT id, tag, note FROM metrics", &[]).expect("select")),
            vec![format!("1|{COLUMN_MARKER}|{INLINE_MARKER}")],
            "both columns must resolve to the values that were inserted"
        );
    }

    // The row image itself IS sealed — the discriminator.
    assert_absent(
        &dir,
        INLINE_MARKER,
        "a DEFAULT-storage column on a table that also uses a columnar column",
    );

    // TODAY'S BEHAVIOUR, PINNED: the columnar batch is on disk in the clear.
    assert!(
        !files_containing(&dir, COLUMN_MARKER).is_empty(),
        "this test pins a KNOWN, DOCUMENTED gap and it just changed. The value of a \
         `STORAGE COLUMNAR` column was NOT found on disk. If the `col:` batches are now sealed, \
         that is an improvement: update `is_row_value_key`'s exclusion list in src/storage/tde.rs, \
         add the prefix to SEALED_PREFIXES here, and invert this assertion. If instead the value \
         no longer takes the columnar path at all, this table's column is not being encoded as a \
         `Value::ColumnarRef` any more."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 10. Reading a non-`data:` keyspace THROUGH a transaction
// ---------------------------------------------------------------------------

/// `Transaction::get` on a key that is not a `data:` row falls through to a raw
/// RocksDB read. `counter:{table}` is written sealed by every route, so that
/// fallback must decode — otherwise the caller is handed ciphertext where it
/// expects a value and the failure surfaces as a bincode error far from its
/// cause.
///
/// This is the direct test for `Transaction::read_raw_decoded`, which nothing
/// else exercises: the SQL surface reads counters through `StorageEngine::get`,
/// so a transaction-scoped read of a sealed non-row keyspace has no other
/// coverage.
#[cfg(feature = "encryption")]
#[test]
fn a_transaction_reads_the_row_counter_as_a_value_not_as_ciphertext() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_TXN_COUNTER_KEY";
    let dir = scratch_dir("txn_counter");
    const ROWS: u64 = 3;

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute("CREATE TABLE tally (id INT PRIMARY KEY, note TEXT)")
            .expect("create table");
        for i in 1..=ROWS {
            db.execute(&format!("INSERT INTO tally VALUES ({i}, 'row {i}')"))
                .expect("insert");
        }
        // Closing flushes every in-memory row counter to its durable
        // `counter:{table}` key, sealed like every other write of it.
    }

    let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
    let txn = db.storage.begin_transaction().expect("begin transaction");

    let counter_key = b"counter:tally".to_vec();
    let raw = txn
        .get(&counter_key)
        .expect("a transaction must be able to read the row counter")
        .expect("the counter key must exist after a clean shutdown");
    let counter: u64 = bincode::deserialize(&raw).unwrap_or_else(|e| {
        panic!(
            "`Transaction::get` returned {} bytes that do not deserialize as the counter ({e}). A \
             sealed value handed back without decoding looks exactly like this.",
            raw.len()
        )
    });
    assert_eq!(
        counter, ROWS,
        "the transaction must see the counter the inserts left behind"
    );
    // …and it must agree with the engine's own decoding read of the same key.
    assert_eq!(
        Some(raw.clone()),
        db.storage.get(&counter_key).expect("engine read of the counter"),
        "`Transaction::get` and `StorageEngine::get` must return the same bytes for the same key"
    );

    // The mirror case: a `data:` row read through the same transaction handle.
    // That arm goes through the MVCC snapshot read rather than the raw fallback,
    // so it covers the other half of `read_at_version`.
    let mut decoded_rows = 0usize;
    for row_id in 1..=ROWS {
        let Some(row) = txn
            .get(&format!("data:tally:{row_id}").into_bytes())
            .expect("a transaction must be able to read a row")
        else {
            continue;
        };
        let tuple: Tuple = bincode::deserialize(&row).unwrap_or_else(|e| {
            panic!(
                "`Transaction::get` returned {} bytes for `data:tally:{row_id}` that do not \
                 deserialize as a tuple ({e})",
                row.len()
            )
        });
        assert_eq!(
            tuple.values.len(),
            2,
            "the decoded row must have the two columns it was inserted with: {:?}",
            tuple.values
        );
        decoded_rows += 1;
    }
    assert_eq!(
        decoded_rows as u64, ROWS,
        "every row must be readable through the transaction handle, decoded"
    );

    txn.rollback().expect("rollback");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 11. RENAME TABLE — a route that SYNTHESIZES values for sealed keyspaces
// ---------------------------------------------------------------------------

/// `Catalog::rename_table` stages the renamed table's schema and row counter
/// into one atomic batch. Both are in-memory plaintext at that point (the schema
/// was just serialized; the counter came back from a decoding read), and both
/// land in keyspaces every other writer seals — so this batch must seal them
/// too. The per-row move in the same batch is a different case and correctly
/// copies stored bytes verbatim, preserving whatever form each row already had.
///
/// The assertion is on the SCHEMA, because it is the one of the two that
/// contains a string a byte scan can look for; the counter is a bincode `u64`
/// with nothing distinguishable in it. Both are sealed by the same
/// `tde::seal` call in the same batch, so the schema standing in for both is a
/// statement about one code path, not two.
///
/// The pre-rename scan is a PRECONDITION, not decoration: the column name must
/// already be absent before the rename, or the post-rename assertion would be
/// reporting on something other than the rename.
#[cfg(feature = "encryption")]
#[test]
fn renaming_a_table_stores_its_schema_in_the_form_every_other_writer_uses() {
    const KEY_VAR: &str = "HELIOSDB_TEST_AT_REST_RENAME_KEY";
    // A column name that can appear only inside a serialized table schema.
    const SCHEMA_MARKER: &str = "qzx_tde_rename_column_6640";
    let dir = scratch_dir("rename_table");

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("encrypted database");
        db.execute(&format!(
            "CREATE TABLE rename_src (id INT PRIMARY KEY, {SCHEMA_MARKER} TEXT)"
        ))
        .expect("create table");
        for i in 1..=3 {
            db.execute(&format!("INSERT INTO rename_src VALUES ({i}, 'row {i}')"))
                .expect("insert");
        }
    }

    assert!(
        files_containing(&dir, SCHEMA_MARKER).is_empty(),
        "PRECONDITION FAILED, and it is not about RENAME: the column name {SCHEMA_MARKER:?} is \
         already on disk in the clear before any rename happened, so something other than \
         `Catalog::rename_table` writes a table schema unsealed. Fix that first — the assertion \
         below cannot say anything until this holds."
    );

    let after_rename = {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen");
        db.execute("ALTER TABLE rename_src RENAME TO rename_dst")
            .expect("rename");
        render(
            &db.query("SELECT id FROM rename_dst ORDER BY id", &[])
                .expect("select from the renamed table"),
        )
    };
    assert_eq!(
        after_rename.len(),
        3,
        "every row must move with the table: {after_rename:?}"
    );

    assert_absent(&dir, SCHEMA_MARKER, "the schema staged by RENAME TABLE");

    // The renamed table must still be usable after a reopen: this is where the
    // moved `counter:` key is read back by `load_counters`, and where a row
    // whose bytes were copied verbatim is read back through the `data:` readers.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&dir, KEY_VAR, KEY_A)).expect("reopen after rename");
        assert_eq!(
            render(
                &db.query("SELECT id FROM rename_dst ORDER BY id", &[])
                    .expect("select after reopen")
            ),
            after_rename,
            "the renamed table's rows must survive a reopen"
        );
        db.execute("INSERT INTO rename_dst VALUES (99, 'after the rename')")
            .expect("insert into the renamed table");
        assert_eq!(
            db.query("SELECT id FROM rename_dst ORDER BY id", &[])
                .expect("select")
                .len(),
            4,
            "a row inserted after the rename must not overwrite one that moved with it"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
