//! Sprinter 66aeb488f312: top-1 exact-match recall of the HNSW wrapper, measured
//! with seeded data so a before/after comparison is like for like.
//!
//! The recall flakes (`physical_standby_fireproof::a_graph_dump_the_sidecar_does_not_describe_is_rebuilt_from_rows`,
//! `vector_sql_integration_test::test_parallel_backfill_indexes_all_rows`,
//! `vector_sql_integration_test::test_create_hnsw_index_on_populated_table_backfills_existing_rows`,
//! once `test_knn_planner_matches_bruteforce_top1`) all ask an index for a row's
//! own vector and do not get the row back. This file measures that miss rate
//! directly: 400 vectors, every row queried with its own vector at k = 1, for
//! each way an index gets built:
//!
//! * `incremental` — one `insert` per row, in row order (INSERT after CREATE INDEX);
//! * `rebuild_from_rows` — one `insert_batch` of every row into a fresh index,
//!   which is what `Catalog::rebuild_vector_indexes` does at open;
//! * `backfill_under_load` — the same batch while every core is busy, which is
//!   CREATE INDEX on a populated table while other work runs (the flaking
//!   tests ran beside the rest of the suite).
//!
//! `search` is what every caller gets. `search_with_ef` is the graph alone (no
//! exact path, no rescue): a miss there is a graph defect even where `search`
//! hides it.
//!
//! The vectors are seeded. hnsw_rs 0.3.3 draws node levels from OS entropy
//! (`LayerGenerator::new`, hnsw.rs:328) and rayon schedules the parallel builds,
//! so the graphs are not: the rates are statistics over many builds, not a replay.
//!
//! Measurement (prints tables, asserts nothing):
//!
//! ```text
//! CARGO_TARGET_DIR=target/r4-measure cargo test --release --test hnsw_recall_measurement \
//!     -- --ignored --nocapture
//! ```
//!
//! It uses only API that predates the R4 fix (`HnswIndex::{new, insert,
//! insert_batch, search, search_with_ef}`, `EmbeddedDatabase`), so the same file
//! runs against the tree before it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]

use heliosdb_nano::vector::{HnswConfig, HnswIndex};
use heliosdb_nano::EmbeddedDatabase;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ROWS: usize = 400;
const DIM: usize = 4;
/// Builds per (dataset, build) cell of the index-level measurement.
const MEASURE_RUNS: u64 = 50;
/// Builds per cell of the engine-level measurement (each one opens a database).
const ENGINE_RUNS: u64 = 10;

/// SplitMix64: a fixed, dependency-free stream.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A multiple of 1/1024 in [0, 1): exact in f32 and in decimal, so a value
    /// printed into SQL parses back to the same bits.
    fn next_dyadic(&mut self) -> f32 {
        (self.next_u64() % 1024) as f32 / 1024.0
    }
}

#[derive(Clone, Copy, Debug)]
enum Dataset {
    /// The spec's loop: uniform random 4-dim vectors.
    Uniform,
    /// The fireproof / durable-index tests' rows: `[v, v + 1, v / 2, 1]` with
    /// `v = id / 4`, all on one line. Seed-independent; the graph still varies.
    Collinear,
}

#[derive(Clone, Copy, Debug)]
enum Build {
    Incremental,
    RebuildFromRows,
    BackfillUnderLoad,
}

fn rows(dataset: Dataset, seed: u64) -> Vec<(u64, Vec<f32>)> {
    match dataset {
        Dataset::Uniform => {
            let mut rng = SplitMix64(seed);
            (0..ROWS)
                .map(|i| (i as u64 + 1, (0..DIM).map(|_| rng.next_dyadic()).collect()))
                .collect()
        }
        Dataset::Collinear => (0..ROWS)
            .map(|i| {
                let v = i as f32 * 0.25;
                (i as u64 + 1, vec![v, v + 1.0, v * 0.5, 1.0])
            })
            .collect(),
    }
}

fn config() -> HnswConfig {
    HnswConfig {
        dimension: DIM,
        ..HnswConfig::default()
    }
}

/// Run `work` while every core spins.
fn under_load<T>(work: impl FnOnce() -> T) -> T {
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..std::thread::available_parallelism().map_or(4, |n| n.get()))
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
                }
            })
        })
        .collect();
    let out = work();
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        spinner.join().unwrap();
    }
    out
}

fn build(mode: Build, rows: &[(u64, Vec<f32>)]) -> HnswIndex {
    let index = HnswIndex::new(config()).unwrap();
    match mode {
        Build::Incremental => {
            for (id, v) in rows {
                index.insert(*id, v).unwrap();
            }
        }
        Build::RebuildFromRows => index.insert_batch(rows).unwrap(),
        Build::BackfillUnderLoad => under_load(|| index.insert_batch(rows)).unwrap(),
    }
    index
}

/// A miss: no row, or a different row at a non-zero distance (a different row
/// at distance 0 holds the same vector and is a correct answer).
fn is_miss(own: u64, hits: &[(u64, f32)]) -> bool {
    match hits.first() {
        None => true,
        Some(&(id, distance)) => id != own && distance > 0.0,
    }
}

#[derive(Default)]
struct Tally {
    builds: u64,
    queries: u64,
    search_misses: u64,
    graph_misses: u64,
    /// Builds in which at least one row was missed by `search`.
    builds_with_a_search_miss: u64,
}

impl Tally {
    fn line(&self, label: &str) -> String {
        let rate = |misses: u64| {
            if self.queries == 0 {
                0.0
            } else {
                100.0 * misses as f64 / self.queries as f64
            }
        };
        let search = format!(
            "search misses {:>5} ({:>7.4}%, in {:>3} builds)",
            self.search_misses,
            rate(self.search_misses),
            self.builds_with_a_search_miss
        );
        let graph = format!(
            "graph-only misses {:>5} ({:>7.4}%)",
            self.graph_misses,
            rate(self.graph_misses)
        );
        format!(
            "{label:<34} builds {:>3}  queries {:>6}  {search}  {graph}",
            self.builds, self.queries
        )
    }
}

fn measure(index: &HnswIndex, rows: &[(u64, Vec<f32>)], tally: &mut Tally) {
    tally.builds += 1;
    let mut missed_in_build = false;
    for (id, v) in rows {
        tally.queries += 1;
        if is_miss(*id, &index.search(v, 1).unwrap()) {
            tally.search_misses += 1;
            missed_in_build = true;
        }
        if is_miss(*id, &index.search_with_ef(v, 1, 200).unwrap()) {
            tally.graph_misses += 1;
        }
    }
    if missed_in_build {
        tally.builds_with_a_search_miss += 1;
    }
}

const DATASETS: [Dataset; 2] = [Dataset::Uniform, Dataset::Collinear];
const BUILDS: [Build; 3] = [Build::Incremental, Build::RebuildFromRows, Build::BackfillUnderLoad];

#[test]
#[ignore = "measurement (Sprinter 66aeb488f312): prints miss rates, asserts nothing"]
fn measure_top1_exact_match_miss_rate() {
    println!(
        "\nHNSW top-1 exact-match misses: {ROWS} rows x {DIM} dims, M = 16, ef_construction = 200, \
         k = 1, graph-only ef = 200, {MEASURE_RUNS} seeded builds per line"
    );
    for dataset in DATASETS {
        for mode in BUILDS {
            let mut tally = Tally::default();
            for run in 0..MEASURE_RUNS {
                let rows = rows(dataset, 0x5EED_0000 + run);
                let index = build(mode, &rows);
                measure(&index, &rows, &mut tally);
            }
            println!("{}", tally.line(&format!("{dataset:?}/{mode:?}")));
        }
    }
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(ToString::to_string).collect();
    format!("[{}]", parts.join(", "))
}

fn insert_rows(db: &EmbeddedDatabase, rows: &[(u64, Vec<f32>)]) {
    for chunk in rows.chunks(100) {
        let values: Vec<String> = chunk
            .iter()
            .map(|(id, v)| format!("({id}, '{}')", vector_literal(v)))
            .collect();
        db.execute(&format!("INSERT INTO r4_rows VALUES {}", values.join(", ")))
            .unwrap();
    }
}

const CREATE_TABLE: &str = "CREATE TABLE r4_rows (id INT8 PRIMARY KEY, emb VECTOR(4))";
const CREATE_INDEX: &str = "CREATE INDEX r4_rows_emb ON r4_rows USING hnsw (emb)";

/// Engine hits carry internal row ids, not the `id` column, so a miss is judged
/// by distance alone: the row's own vector is in the index at distance 0, and
/// any hit at distance 0 holds that same vector (the values are dyadic, so the
/// SQL literal parses back to the same bits).
fn engine_misses(db: &EmbeddedDatabase, rows: &[(u64, Vec<f32>)]) -> u64 {
    let indexes = db.storage.vector_indexes();
    rows.iter()
        .filter(|(_, v)| {
            let hits = indexes.search("r4_rows_emb", v, 1).unwrap();
            hits.first().is_none_or(|&(_, distance)| distance > 0.0)
        })
        .count() as u64
}

/// The same measurement through the real CREATE INDEX / INSERT / reopen paths
/// (`search` only: the engine exposes no graph-only search).
#[test]
#[ignore = "measurement (Sprinter 66aeb488f312): prints miss rates, asserts nothing"]
fn measure_top1_exact_match_miss_rate_through_the_engine() {
    println!("\nThrough the engine: {ROWS} uniform rows x {DIM} dims, {ENGINE_RUNS} seeded builds per line");
    let mut incremental = Tally::default();
    let mut backfill = Tally::default();
    let mut reopen = Tally::default();
    for run in 0..ENGINE_RUNS {
        let rows = rows(Dataset::Uniform, 0xE461_0000 + run);
        let queries = rows.len() as u64;

        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(CREATE_TABLE).unwrap();
        db.execute(CREATE_INDEX).unwrap();
        insert_rows(&db, &rows);
        let misses = engine_misses(&db, &rows);
        incremental.builds += 1;
        incremental.queries += queries;
        incremental.search_misses += misses;
        incremental.builds_with_a_search_miss += u64::from(misses > 0);
        drop(db);

        let dir = tempfile::tempdir().unwrap();
        {
            let db = EmbeddedDatabase::new(dir.path()).unwrap();
            db.execute(CREATE_TABLE).unwrap();
            insert_rows(&db, &rows);
            db.execute(CREATE_INDEX).unwrap();
            let misses = engine_misses(&db, &rows);
            backfill.builds += 1;
            backfill.queries += queries;
            backfill.search_misses += misses;
            backfill.builds_with_a_search_miss += u64::from(misses > 0);
            // Close without a graph dump: the next open rebuilds from rows.
            db.storage.set_index_snapshots_on_close(false);
        }
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        assert_eq!(db.storage.last_index_open_report().unwrap().vector_rebuilt, 1);
        let misses = engine_misses(&db, &rows);
        reopen.builds += 1;
        reopen.queries += queries;
        reopen.search_misses += misses;
        reopen.builds_with_a_search_miss += u64::from(misses > 0);
    }
    println!("{}", incremental.line("engine/insert after CREATE INDEX"));
    println!("{}", backfill.line("engine/CREATE INDEX backfill"));
    println!("{}", reopen.line("engine/rebuild from rows at open"));
}

/// The fix, held strictly: `search` answers every row's own vector with that
/// row, whatever built the index. Red before R4 whenever a build left a row
/// unreachable (the measurement above gives the rate, so this is not red on
/// every run); never red after, because at or below `EXACT_SEARCH_MAX_POINTS`
/// the answer is exact.
#[test]
fn search_finds_every_rows_own_vector_for_every_build_mode() {
    for dataset in DATASETS {
        for mode in BUILDS {
            let rows = rows(dataset, 0xC0FF_EE00);
            let index = build(mode, &rows);
            let missed: Vec<u64> = rows
                .iter()
                .filter(|(id, v)| is_miss(*id, &index.search(v, 1).unwrap()))
                .map(|(id, _)| *id)
                .collect();
            assert!(
                missed.is_empty(),
                "{dataset:?}/{mode:?}: rows not returned for their own vector: {missed:?}"
            );
        }
    }
}
