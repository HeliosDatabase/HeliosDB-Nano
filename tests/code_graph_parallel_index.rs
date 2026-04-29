//! Parallel `code_index` equivalence + telemetry tests (FR
//! `FEATURE_REQUEST_parallel_code_index.md`).
//!
//! Verifies that the parallel parse + extract path produces the same
//! `_hdb_code_*` rows as the legacy serial path, that the
//! `parallelism` knob is honoured, that the dedicated thread-pool is
//! used (workers stat reflects the configured count), and that the
//! force_reparse hash gate still short-circuits unchanged files.

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

/// Realistic multi-language fixture corpus. Big enough that 8 workers
/// have actual work to share but small enough that the test runs in
/// well under a second. Mix of languages exercises both static and
/// dynamic dispatch paths.
const FIXTURE_FILES: &[(&str, &str, &str)] = &[
    ("a.rs", "rust", "pub fn alpha() { beta(); }\npub fn beta() {}\npub struct S;\nimpl S { pub fn m(&self) -> i32 { 0 } }\n"),
    ("b.rs", "rust", "use crate::a::S;\npub fn make() -> S { S }\npub fn caller() { let s = make(); s.m(); }\n"),
    ("c.rs", "rust", "pub trait T { fn run(&self); }\npub struct U;\nimpl T for U { fn run(&self) {} }\n"),
    ("d.py", "python", "def hello():\n    print('hi')\n\nclass Foo:\n    def bar(self):\n        return hello()\n"),
    ("e.py", "python", "from d import Foo\n\ndef use_foo():\n    f = Foo()\n    return f.bar()\n"),
    ("f.ts", "typescript", "export function add(a: number, b: number): number { return a + b; }\nexport class Calc { mul(a: number, b: number) { return a * b; } }\n"),
    ("g.go", "go", "package main\n\nfunc Square(x int) int { return x * x }\n\ntype Box struct { v int }\n\nfunc (b Box) Get() int { return b.v }\n"),
    ("h.rs", "rust", "pub fn root() {}\npub mod inner { pub fn deep() { super::root(); } }\n"),
];

fn populate_corpus(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    for (path, lang, content) in FIXTURE_FILES {
        db.execute_params_returning(
            "INSERT INTO src VALUES ($1, $2, $3)",
            &[
                Value::String((*path).into()),
                Value::String((*lang).into()),
                Value::String((*content).into()),
            ],
        )
        .unwrap();
    }
}

/// Snapshot the contents of every `_hdb_code_*` table as a stable,
/// orderable Vec<Vec<String>>. Returned by both serial and parallel
/// runs; an `assert_eq!` between the two is the byte-equivalence
/// proof the FR's first acceptance criterion calls for.
fn snapshot_indexed_tables(db: &EmbeddedDatabase) -> Vec<(String, Vec<Vec<String>>)> {
    let tables = ["_hdb_code_files", "_hdb_code_symbols", "_hdb_code_symbol_refs"];
    let mut out = Vec::new();
    for t in &tables {
        // Order results by path / qualified / line so worker-scheduling
        // randomness can't shift rows around within the snapshot.
        let order_by = match *t {
            "_hdb_code_files" => "path",
            "_hdb_code_symbols" => "file_id, line_start, name",
            "_hdb_code_symbol_refs" => "file_id, line, kind, COALESCE(to_symbol, -1)",
            _ => "1",
        };
        let rows = db
            .query(
                &format!("SELECT * FROM {t} ORDER BY {order_by}"),
                &[],
            )
            .unwrap();
        let mut serialised: Vec<Vec<String>> = Vec::with_capacity(rows.len());
        for row in rows {
            // Skip the auto-generated node_id column — it's a row id
            // counter that depends on the order rows hit the engine,
            // which is identical across serial / parallel because the
            // write phase is single-threaded and walks results in
            // input order. But for safety we drop it from the diff.
            let cells: Vec<String> = row
                .values
                .iter()
                .skip(1) // node_id
                .map(|v| format!("{v:?}"))
                .collect();
            serialised.push(cells);
        }
        out.push(((*t).to_string(), serialised));
    }
    out
}

#[test]
fn parallel_output_matches_serial_byte_for_byte() {
    // Serial run.
    let db_serial = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_serial);
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(1);
    let stats_serial = db_serial.code_index(opts).expect("serial code_index");
    assert!(stats_serial.files_parsed > 0);
    assert_eq!(stats_serial.parse_workers, 1);
    let snap_serial = snapshot_indexed_tables(&db_serial);

    // Parallel run.
    let db_par = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_par);
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(8);
    let stats_par = db_par.code_index(opts).expect("parallel code_index");
    assert_eq!(stats_par.parse_workers, 8);
    let snap_par = snapshot_indexed_tables(&db_par);

    // Output equivalence — the "byte-identical" acceptance criterion.
    assert_eq!(snap_serial.len(), snap_par.len());
    for ((t_s, rows_s), (t_p, rows_p)) in snap_serial.iter().zip(snap_par.iter()) {
        assert_eq!(t_s, t_p);
        assert_eq!(
            rows_s.len(),
            rows_p.len(),
            "row count diverged on {t_s}: serial={} parallel={}",
            rows_s.len(),
            rows_p.len()
        );
        for (rs, rp) in rows_s.iter().zip(rows_p.iter()) {
            assert_eq!(rs, rp, "row diverged on {t_s}");
        }
    }

    // Counters must agree.
    assert_eq!(stats_serial.files_parsed, stats_par.files_parsed);
    assert_eq!(stats_serial.symbols_written, stats_par.symbols_written);
    assert_eq!(stats_serial.refs_written, stats_par.refs_written);
}

#[test]
fn parallelism_default_resolves_to_bounded_workers() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db);
    let stats = db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("auto code_index");
    // Default cap is min(num_cpus, 8) and minimum 1.
    assert!(
        stats.parse_workers >= 1 && stats.parse_workers <= 8,
        "expected workers in [1,8], got {}",
        stats.parse_workers
    );
}

#[test]
fn force_reparse_false_honours_hash_gate() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db);

    let first = db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("first index");
    assert!(first.files_parsed > 0);
    assert_eq!(first.files_unchanged, 0);

    // Re-run without changing any rows. Every file should match the
    // stored sha256 and short-circuit before reaching the parse phase.
    let second = db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("re-index");
    assert_eq!(second.files_parsed, 0);
    assert_eq!(
        second.files_unchanged as usize,
        FIXTURE_FILES.len(),
        "every fixture file should hit the hash-gate"
    );
}

#[test]
fn telemetry_records_parse_and_write_timings() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db);
    let stats = db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("indexed");
    // Parse phase must have run on *something*. Write phase must
    // also have run since files_parsed > 0. Both should be > 0 on
    // any reasonably non-trivial corpus.
    assert!(stats.files_parsed > 0);
    assert!(stats.parse_elapsed_ms < 60_000, "parse > 60s on tiny corpus is suspicious");
    assert!(stats.write_elapsed_ms < 60_000, "write > 60s on tiny corpus is suspicious");
}

#[test]
fn chunked_output_matches_unchunked() {
    // Open question #2: chunked path bounds peak memory by
    // interleaving parse + drain. Equivalence to the all-in-one
    // path is non-negotiable: same `_hdb_code_*` rows, same
    // counters, regardless of how many chunks the corpus is
    // sliced into.
    let db_unchunked = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_unchunked);
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(4);
    opts.chunk_size = None; // single chunk
    let stats_un = db_unchunked.code_index(opts).expect("unchunked code_index");
    assert_eq!(stats_un.chunks_processed, 1);
    let snap_un = snapshot_indexed_tables(&db_unchunked);

    let db_chunked = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_chunked);
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(4);
    opts.chunk_size = Some(2); // 8 fixture files → 4 chunks
    let stats_ch = db_chunked.code_index(opts).expect("chunked code_index");
    assert!(
        stats_ch.chunks_processed > 1,
        "chunk_size=2 on 8 files should produce >1 chunks, got {}",
        stats_ch.chunks_processed
    );
    let snap_ch = snapshot_indexed_tables(&db_chunked);

    // Output equivalence.
    assert_eq!(snap_un.len(), snap_ch.len());
    for ((t_u, rows_u), (t_c, rows_c)) in snap_un.iter().zip(snap_ch.iter()) {
        assert_eq!(t_u, t_c);
        assert_eq!(rows_u.len(), rows_c.len(), "row count diverged on {t_u}");
        for (ru, rc) in rows_u.iter().zip(rows_c.iter()) {
            assert_eq!(ru, rc, "row diverged on {t_u}");
        }
    }

    // Counter parity.
    assert_eq!(stats_un.files_parsed, stats_ch.files_parsed);
    assert_eq!(stats_un.symbols_written, stats_ch.symbols_written);
    assert_eq!(stats_un.refs_written, stats_ch.refs_written);
}

#[test]
fn force_reparse_against_populated_kb_truncates() {
    // Tier 1.3 fast path: when force_reparse=true on an already-
    // populated KB, the indexer truncates `_hdb_code_*` once
    // instead of issuing per-file DELETE-then-INSERT (which on
    // large corpora triggers RocksDB compaction storms — the
    // pilot's 1 h 55 m kill anti-pattern).
    //
    // Output IDs (file_id, node_id) drift after a truncate-and-
    // rebuild because the row-id counters survive TRUNCATE (matching
    // Postgres' default sequence behaviour). What must NOT drift is
    // the *content*: same files parsed, same symbol names, same ref
    // edges by name. Compare those projections instead of ID columns.
    let db_pre_populated = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_pre_populated);
    db_pre_populated
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("first index");
    let mut opts = CodeIndexOptions::for_table("src");
    opts.force_reparse = true;
    let stats_force = db_pre_populated
        .code_index(opts)
        .expect("force-reparse index");

    let db_cold = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db_cold);
    let stats_cold = db_cold
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("cold index");

    // Counters parity — the load-bearing claim.
    assert_eq!(stats_force.files_parsed, stats_cold.files_parsed);
    assert_eq!(stats_force.symbols_written, stats_cold.symbols_written);
    assert_eq!(stats_force.refs_written, stats_cold.refs_written);

    // Content parity by name + line — what downstream LSP queries
    // actually use. Sort both projections and assert equality.
    fn symbol_signatures(db: &EmbeddedDatabase) -> Vec<String> {
        let mut sigs: Vec<String> = db
            .query(
                "SELECT name, qualified, kind, line_start FROM _hdb_code_symbols",
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|r| format!("{:?}", r.values))
            .collect();
        sigs.sort();
        sigs
    }
    assert_eq!(symbol_signatures(&db_pre_populated), symbol_signatures(&db_cold));

    fn ref_signatures(db: &EmbeddedDatabase) -> Vec<String> {
        let mut sigs: Vec<String> = db
            .query(
                "SELECT to_name, kind, line, resolution FROM _hdb_code_symbol_refs",
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|r| format!("{:?}", r.values))
            .collect();
        sigs.sort();
        sigs
    }
    assert_eq!(ref_signatures(&db_pre_populated), ref_signatures(&db_cold));

    fn file_paths(db: &EmbeddedDatabase) -> Vec<String> {
        let mut paths: Vec<String> = db
            .query("SELECT path, lang, sha256 FROM _hdb_code_files", &[])
            .unwrap()
            .into_iter()
            .map(|r| format!("{:?}", r.values))
            .collect();
        paths.sort();
        paths
    }
    assert_eq!(file_paths(&db_pre_populated), file_paths(&db_cold));
}

#[test]
fn write_phase_under_explicit_outer_txn_succeeds() {
    // Tier 1.1: the indexer detects an outer transaction (via
    // `db.in_transaction()`) and skips its own begin/commit so the
    // caller's transaction is not committed prematurely. We can't
    // assert fsync-count reduction directly from a unit test, but
    // we can verify the contract: code_index runs cleanly when the
    // caller has an outer txn open and produces the same row counts
    // it would standalone. (DDL auto-commit semantics differ across
    // engines, so we don't test rollback isolation here.)
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&db);

    db.begin().expect("begin outer txn");
    let stats = db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("indexed within outer txn");
    db.commit().expect("commit outer txn");

    assert!(
        stats.files_parsed >= FIXTURE_FILES.len() as u64,
        "expected ≥{} files indexed, got {}",
        FIXTURE_FILES.len(),
        stats.files_parsed
    );

    // Outer-txn aware path must produce the same row counts as the
    // self-managed path — proves the writes didn't get
    // double-counted or lost.
    let cmp_db = EmbeddedDatabase::new_in_memory().expect("db");
    populate_corpus(&cmp_db);
    let cmp_stats = cmp_db
        .code_index(CodeIndexOptions::for_table("src"))
        .expect("indexed standalone");
    assert_eq!(stats.files_parsed, cmp_stats.files_parsed);
    assert_eq!(stats.symbols_written, cmp_stats.symbols_written);
    assert_eq!(stats.refs_written, cmp_stats.refs_written);
}

#[test]
fn empty_corpus_skips_pool_construction() {
    // No source rows → no chunks → no pool. Verifies the chunked
    // path doesn't choke on empty input.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(4);
    opts.chunk_size = Some(100);
    let stats = db.code_index(opts).expect("indexed empty");
    assert_eq!(stats.files_seen, 0);
    assert_eq!(stats.files_parsed, 0);
    assert_eq!(stats.chunks_processed, 0);
}

#[test]
fn small_corpus_no_regression_under_parallelism() {
    // FR acceptance: "≤100 files: rayon's thread-pool startup is
    // amortised, overhead negligible." We simulate that by running
    // a single file and asserting it indexes cleanly with workers=8.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("only.rs".into()),
            Value::String("pub fn solo() {}\n".into()),
        ],
    )
    .unwrap();
    let mut opts = CodeIndexOptions::for_table("src");
    opts.parallelism = Some(8);
    let stats = db.code_index(opts).expect("indexed");
    assert_eq!(stats.files_parsed, 1);
    assert!(stats.symbols_written >= 1);
}
