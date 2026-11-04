//! Spec R2 (`docs/plans/specs/R2_ddl_window_cut.md`) — plan decision 7
//! (`docs/plans/PHYSICAL_STANDBY_REPLICATION.md` "Decisions" 7 and §D3
//! "Consistent cut"): does a reader of a physical standby ever see a TORN
//! state while its primary runs DDL — an answer that matches no state the
//! primary passed through at a statement boundary?
//!
//! It does, on BOTH nodes: a statement is not atomic on the primary (the spec's
//! "Phase 1 result": a DDL statement is several write groups — ADD COLUMN
//! DEFAULT one per rewritten row — and nothing keeps a reader of either node
//! out between them). Cut markers on the standby were tried and rejected (the
//! spec's "Decision 2026-10-31: no cut markers"). The fix is R5, statement
//! atomicity on the primary (`docs/plans/specs/R5_statement_atomicity.md`), and
//! the main test here is R5's acceptance: it is `#[ignore]`d until R5 lands.
//! The split self-check stays active: it proves the detector fires.
//!
//! # The workload (real managed daemons, `tests/support/physical_pair.rs`)
//!
//! A writer on the PRIMARY runs N iterations of [`script`]: CREATE TABLE, a
//! multi-row INSERT, CREATE INDEX; ALTER TABLE ADD COLUMN with a DEFAULT on
//! the populated table; DROP COLUMN (a middle one); RENAME COLUMN (the
//! indexed one); CREATE INDEX USING hnsw, an INSERT it indexes, DROP INDEX;
//! RENAME TABLE; TRUNCATE and an INSERT after it; an explicit transaction
//! mixing DDL and DML (CREATE TABLE, INSERT, UPDATE, ADD COLUMN with a
//! DEFAULT, DELETE, COMMIT); DROP TABLE. Rows carry the iteration number, so
//! no two iterations' states are equal.
//!
//! Readers on the STANDBY run [`queries`] in a tight loop for the whole test
//! — per table, a full scan, `COUNT(*)`, PRIMARY KEY point reads, reads
//! through the secondary index (by its column's old and new name), a kNN
//! read, `information_schema.columns`, and the full scan again — recording
//! every answer. Three of them add a unique trailing comment to every query
//! (no result or plan cache answers them: they read the store and its
//! derived state); one asks the same text every time, as an application
//! does (the caches answer it). One more reader runs on the PRIMARY as a
//! control, held to the same assertions: a statement must be atomic for the
//! primary's own readers too.
//!
//! # The model: the states the primary passed through
//!
//! After every statement — inside the explicit transaction too, where DDL is
//! not transactional — an OBSERVER session on the primary (not the writer's)
//! asks every reader query and records the answers: state `k` is the primary
//! as any other session saw it after statement `k`; state 0 is before the
//! first. What a statement shows between two of its own write groups is
//! deliberately NOT a state: that is what a torn read is.
//!
//! # The detector
//!
//! Every answer gets a WINDOW of states it may legitimately show, from wall
//! clocks and replication positions — never from the order of the answers
//! themselves (a greedy order, phase 1's, cascades: an answer that carries no
//! iteration number matches a state in every iteration):
//!
//! * the upper end: the statements the writer had SENT before the answer
//!   arrived (nothing later can be visible anywhere);
//! * the lower end, on the primary: the statements whose answer came back
//!   before the query was sent; on the standby, for the readers that bypass
//!   the caches: the statements whose last write group the standby had
//!   applied when the reader started the table's round (its
//!   `pg_last_wal_replay_lsn()`, which moves when a group is written, before
//!   its publication: an answer older than that lags the store); for the
//!   cached reader: none (a cached answer is an older state by design).
//!
//! Classes:
//!
//! * **torn**: an answer equal to the primary's answer to the same query in
//!   NO recorded state (a kNN answer may instead be any non-empty subset of
//!   that state's rows, at most the limit, where the primary's own answer is
//!   non-empty: HNSW is approximate). This covers the spec's list: a table
//!   with rows but no columns or the reverse, rows of a width matching
//!   neither schema, a `COUNT(*)` no state had, an internal error — errors
//!   are answers: "relation does not exist" (42P01) and "column does not
//!   exist" (42703) match the states in which the primary said so too,
//!   anything else matches nothing;
//! * **window**: an answer that matches states, but none inside its window
//!   (a stale index-backed read, or one ahead of anything sent);
//! * **pair**: the full scans that open and close a table's queries in one
//!   round agree and pin a contiguous run of states, and a query between
//!   them matches none of those states — `COUNT(*)` disagreeing with
//!   `SELECT *` in the same snapshot, an index-backed read disagreeing with a
//!   full scan;
//! * **broken** / **not ready**: a connection failure (or no answer within
//!   [`QUERY_BUDGET`]) and SQLSTATE 55000; besides, a panic in either
//!   daemon's log and a standby not ready at the end fail the run.
//!
//! The acceptance asserts all five classes zero on every reader, standby and
//! primary control alike ([`Outcome::assert_clean`]).
//!
//! # The census
//!
//! The primary archives its write groups (`[replication.archive]`, a segment
//! sealed every second). After the plain run every archived group is
//! attributed to the statement whose sequence interval holds it, and the
//! test logs (lines prefixed `CENSUS`) how many groups each statement
//! produced in each iteration and every group of the first iteration with
//! the key classes it writes.
//!
//! # The self-check and the paced run (`internal-tests`)
//!
//! [`the_detector_reports_a_write_group_split_by_the_test_hook`] runs the
//! workload with the standby's applier writing every write group that
//! carries two or more `ddl_a` rows in two halves, readers admitted between
//! them (`physical_applier::SPLIT_GROUP_ENV`): half of one write group is a
//! state the primary never passed through, and the detector must report it —
//! so a clean run is not a vacuous one. It runs without the primary control
//! reader: it asserts nothing about the primary, and that reader can trigger
//! the schema-cache race of the repro's finding (b) until R5 fixes it.
//! [`standby_readers_never_see_a_torn_state_when_every_write_group_is_published_on_its_own`]
//! runs it with every write group applied and published on its own and held
//! open for readers (`physical_applier::APPLY_PACE_ENV`): every state between
//! two groups of a multi-group statement is exposed; `#[ignore]`d until R5,
//! like the plain run.
//!
//! Run: `cargo test --features internal-tests --test physical_ddl_window --
//! --nocapture` runs the self-check (a default build has no active test here).
//! R5's acceptance, the plain run with the census and, with `internal-tests`,
//! the paced run: add `--ignored`. `HELIOSDB_TEST_DDL_WINDOW_ITERATIONS` sets
//! the plain run's iteration count.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::physical_applier::{decode_raw_batch, RawOp};
#[cfg(feature = "internal-tests")]
use heliosdb_nano::replication::physical_applier::{APPLY_PACE_ENV, SPLIT_GROUP_ENV};
use heliosdb_nano::storage::keyspace::{classify, KeyClass};
use physical_pair::*;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::{Client, SimpleQueryMessage};

/// Iterations of the plain run; [`ITERATIONS_ENV`] overrides.
const ITERATIONS: usize = 20;
const ITERATIONS_ENV: &str = "HELIOSDB_TEST_DDL_WINDOW_ITERATIONS";
#[cfg(feature = "internal-tests")]
const PACED_ITERATIONS: usize = 6;
/// How long the paced standby holds each published write group.
#[cfg(feature = "internal-tests")]
const PACE_MS: u64 = 40;
#[cfg(feature = "internal-tests")]
const SPLIT_ITERATIONS: usize = 4;
/// How long readers see the first half of a split write group.
#[cfg(feature = "internal-tests")]
const SPLIT_PAUSE_MS: u64 = 500;
/// Rows the first INSERT of an iteration writes, in one statement.
const ROWS: usize = 12;
const KNN_LIMIT: usize = 3;
const PROBE: &str = "[0.5, 0.5, 0.5, 0.5]";
/// One answer's budget: a reader that waits longer reports its connection broken.
const QUERY_BUDGET: Duration = Duration::from_secs(30);
/// The primary archives every write group and seals a segment every second:
/// the census reads the groups back from the segments.
const PRIMARY_CONFIG: &str = "[replication.archive]\nenabled = true\nsegment_max_secs = 1\npoll_ms = 100\n";
const TABLES: [&str; 3] = ["ddl_a", "ddl_b", "ddl_c"];
/// Violations printed per reader in the summary.
const SHOWN_PER_READER: usize = 8;
/// What a standby reader asks at the start of each table's round: the lower
/// end of the window of that round's answers.
const REPLAY_POSITION_SQL: &str = "SELECT pg_last_wal_replay_lsn()::text";

// ---------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------

/// One statement of the writer's script.
struct Step {
    label: &'static str,
    sql: String,
}

/// A row's vector: distinct per row, a function of the row id and the iteration.
fn vector(id: usize, iteration: usize) -> String {
    let component = |numerator: usize, denominator: usize| numerator as f64 / denominator as f64;
    format!(
        "[{:.4}, {:.4}, {:.4}, {:.4}]",
        component(id % 7, 8),
        component((id * 3) % 11, 16),
        component((id + iteration) % 5, 8),
        component(id, 32)
    )
}

/// One iteration of the writer: every DDL shape of the spec, on tables whose
/// rows carry `it`. Each step is one statement (the explicit transaction is
/// one step per statement, so the observer records what other sessions see
/// while it is open).
fn script(it: usize) -> Vec<Step> {
    let rows: Vec<String> = (1..=ROWS)
        .map(|id| format!("({id}, {it}, 'm{id}', 'v{id}', '{}')", vector(id, it)))
        .collect();
    let step = |label: &'static str, sql: String| Step { label, sql };
    vec![
        step(
            "create_table",
            "CREATE TABLE ddl_a (id INT PRIMARY KEY, it INT NOT NULL, mid TEXT, v TEXT, emb VECTOR(4))".to_string(),
        ),
        step("insert_rows", format!("INSERT INTO ddl_a VALUES {}", rows.join(", "))),
        step("create_index", "CREATE INDEX ddl_a_v ON ddl_a (v)".to_string()),
        step(
            "add_column_default",
            "ALTER TABLE ddl_a ADD COLUMN d INT DEFAULT 7".to_string(),
        ),
        step("drop_column", "ALTER TABLE ddl_a DROP COLUMN mid".to_string()),
        step("rename_column", "ALTER TABLE ddl_a RENAME COLUMN v TO w".to_string()),
        step(
            "create_hnsw",
            "CREATE INDEX ddl_a_emb ON ddl_a USING hnsw (emb)".to_string(),
        ),
        step(
            "insert_with_hnsw",
            format!(
                "INSERT INTO ddl_a (id, it, w, emb, d) VALUES (13, {it}, 'v13', '{}', 8)",
                vector(13, it)
            ),
        ),
        step("drop_hnsw", "DROP INDEX ddl_a_emb".to_string()),
        step("rename_table", "ALTER TABLE ddl_a RENAME TO ddl_b".to_string()),
        step("truncate", "TRUNCATE TABLE ddl_b".to_string()),
        step(
            "insert_after_truncate",
            format!(
                "INSERT INTO ddl_b (id, it, w, emb, d) VALUES (21, {it}, 'v21', '{}', 9), (22, {it}, 'v22', '{}', 10)",
                vector(21, it),
                vector(22, it)
            ),
        ),
        step("txn_begin", "BEGIN".to_string()),
        step(
            "txn_create_table",
            "CREATE TABLE ddl_c (id INT PRIMARY KEY, it INT NOT NULL, note TEXT)".to_string(),
        ),
        // The rows go in BEFORE the ADD COLUMN … DEFAULT of the same
        // transaction, on purpose. They stay narrow today: a primary-key point
        // read of one answers 3 values under a 4-column schema, fails `Column
        // index 3 out of bounds in tuple` on the wire's general path and sends
        // a DataRow the client cannot parse on its fast path, ending that
        // connection (finding (c), `tests/physical_ddl_window_repro.rs`). R5
        // fixes it, and this order is part of its acceptance.
        step(
            "txn_insert",
            format!("INSERT INTO ddl_c VALUES (1, {it}, 'c1'), (2, {it}, 'c2')"),
        ),
        step("txn_update", "UPDATE ddl_b SET w = 'u21' WHERE id = 21".to_string()),
        step(
            "txn_add_column_default",
            "ALTER TABLE ddl_c ADD COLUMN e INT DEFAULT 5".to_string(),
        ),
        step("txn_delete", "DELETE FROM ddl_b WHERE id = 22".to_string()),
        step("txn_commit", "COMMIT".to_string()),
        step("drop_table_b", "DROP TABLE ddl_b".to_string()),
        step("drop_table_c", "DROP TABLE ddl_c".to_string()),
    ]
}

/// A reader query. `all` is the index of its table's full scan: the kNN check
/// reads that state's rows.
struct Query {
    name: &'static str,
    table: &'static str,
    sql: String,
    knn: bool,
    all: usize,
}

fn queries() -> Vec<Query> {
    let knn_sql = |table: &str| {
        format!("SELECT id, it FROM {table} WHERE emb IS NOT NULL ORDER BY emb <-> '{PROBE}' LIMIT {KNN_LIMIT}")
    };
    let columns_sql = |table: &str| {
        format!(
            "SELECT column_name, data_type, ordinal_position FROM information_schema.columns WHERE table_name = \
             '{table}'"
        )
    };
    let specs: Vec<(&'static str, &'static str, String, bool)> = vec![
        ("all", "ddl_a", "SELECT * FROM ddl_a".to_string(), false),
        ("count", "ddl_a", "SELECT COUNT(*) FROM ddl_a".to_string(), false),
        ("pk 3", "ddl_a", "SELECT * FROM ddl_a WHERE id = 3".to_string(), false),
        (
            "index on v",
            "ddl_a",
            "SELECT id FROM ddl_a WHERE v = 'v5'".to_string(),
            false,
        ),
        (
            "index on w",
            "ddl_a",
            "SELECT id FROM ddl_a WHERE w = 'v5'".to_string(),
            false,
        ),
        ("knn", "ddl_a", knn_sql("ddl_a"), true),
        ("columns", "ddl_a", columns_sql("ddl_a"), false),
        ("all", "ddl_b", "SELECT * FROM ddl_b".to_string(), false),
        ("count", "ddl_b", "SELECT COUNT(*) FROM ddl_b".to_string(), false),
        ("pk 3", "ddl_b", "SELECT * FROM ddl_b WHERE id = 3".to_string(), false),
        ("pk 21", "ddl_b", "SELECT * FROM ddl_b WHERE id = 21".to_string(), false),
        (
            "index on w",
            "ddl_b",
            "SELECT id FROM ddl_b WHERE w = 'v5'".to_string(),
            false,
        ),
        ("knn", "ddl_b", knn_sql("ddl_b"), true),
        ("columns", "ddl_b", columns_sql("ddl_b"), false),
        ("all", "ddl_c", "SELECT * FROM ddl_c".to_string(), false),
        ("count", "ddl_c", "SELECT COUNT(*) FROM ddl_c".to_string(), false),
        ("pk 1", "ddl_c", "SELECT * FROM ddl_c WHERE id = 1".to_string(), false),
        ("columns", "ddl_c", columns_sql("ddl_c"), false),
    ];
    let scans: Vec<(&'static str, usize)> = specs
        .iter()
        .enumerate()
        .filter(|(_, spec)| spec.0 == "all")
        .map(|(index, spec)| (spec.1, index))
        .collect();
    specs
        .into_iter()
        .map(|(name, table, sql, knn)| Query {
            name,
            table,
            sql,
            knn,
            all: scans.iter().find(|(scanned, _)| *scanned == table).unwrap().1,
        })
        .collect()
}

/// One reader round: per table, its full scan, its other queries, and its
/// full scan again (the pair check's sandwich).
fn round_plan(queries: &[Query]) -> Vec<Vec<usize>> {
    TABLES
        .iter()
        .map(|table| {
            let all = queries
                .iter()
                .position(|query| query.table == *table && query.name == "all")
                .unwrap();
            let mut order = vec![all];
            order.extend((0..queries.len()).filter(|&index| queries[index].table == *table && index != all));
            order.push(all);
            order
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// One answer, canonical: rows sorted (a set), errors by family.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Answer {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// 42P01: "relation does not exist".
    MissingRelation,
    /// 42703: "column does not exist".
    MissingColumn,
    Error {
        sqlstate: String,
        message: String,
    },
    /// No answer: the connection failed, or nothing came within [`QUERY_BUDGET`].
    Broken(String),
}

impl Answer {
    fn brief(&self) -> String {
        match self {
            Self::Rows { columns, rows } => {
                let shown: Vec<String> = rows
                    .iter()
                    .take(4)
                    .map(|row| {
                        let values: Vec<String> = row
                            .iter()
                            .map(|value| abbreviate(value.as_deref().unwrap_or("NULL"), 24))
                            .collect();
                        format!("({})", values.join(", "))
                    })
                    .collect();
                format!(
                    "{} row(s) of [{}] {}{}",
                    rows.len(),
                    columns.join(", "),
                    shown.join(" "),
                    if rows.len() > 4 { " ..." } else { "" }
                )
            }
            Self::MissingRelation => "42P01 relation does not exist".to_string(),
            Self::MissingColumn => "42703 column does not exist".to_string(),
            Self::Error { sqlstate, message } => format!("{sqlstate} {}", abbreviate(message, 160)),
            Self::Broken(why) => format!("no answer: {}", abbreviate(why, 160)),
        }
    }

    fn not_ready(&self) -> bool {
        matches!(self, Self::Error { sqlstate, .. } if sqlstate == "55000")
    }
}

fn abbreviate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(limit).collect::<String>())
    }
}

/// Ask `sql` (simple protocol) and canonicalize the answer.
async fn ask(client: &Client, sql: &str) -> Answer {
    let messages = match tokio::time::timeout(QUERY_BUDGET, client.simple_query(sql)).await {
        Err(_) => return Answer::Broken(format!("no answer within {QUERY_BUDGET:?}")),
        Ok(Err(error)) => {
            return match error.as_db_error() {
                None => Answer::Broken(error.to_string()),
                Some(db) => match db.code().code() {
                    "42P01" => Answer::MissingRelation,
                    "42703" => Answer::MissingColumn,
                    // A message that quotes the statement must not carry
                    // the asker's unique comment ([`tagged`]).
                    code => Answer::Error {
                        sqlstate: code.to_string(),
                        message: db.message().split(" -- ").next().unwrap_or_default().to_string(),
                    },
                },
            };
        }
        Ok(Ok(messages)) => messages,
    };
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for message in &messages {
        match message {
            SimpleQueryMessage::RowDescription(description) => {
                columns = description.iter().map(|column| column.name().to_string()).collect();
            }
            SimpleQueryMessage::Row(row) => {
                if columns.is_empty() {
                    columns = row.columns().iter().map(|column| column.name().to_string()).collect();
                }
                rows.push((0..row.len()).map(|index| row.get(index).map(str::to_string)).collect());
            }
            _ => {}
        }
    }
    rows.sort();
    Answer::Rows { columns, rows }
}

/// The one text value of a one-row answer, if it is an LSN.
fn replay_position(answer: &Answer) -> Option<u64> {
    match answer {
        Answer::Rows { rows, .. } => match rows.first().and_then(|row| row.first()) {
            Some(Some(text)) if text.contains('/') => Some(lsn(text)),
            _ => None,
        },
        _ => None,
    }
}

/// `sql` with a unique trailing comment: no result or plan cache keyed by the
/// text answers it.
fn tagged(sql: &str, tag: &str, n: u64) -> String {
    format!("{sql} -- {tag}.{n}")
}

/// Answers by id, so that tens of thousands of observations share them.
#[derive(Default)]
struct Interner {
    ids: HashMap<Answer, u32>,
    list: Vec<Answer>,
}

impl Interner {
    fn intern(&mut self, answer: Answer) -> u32 {
        if let Some(id) = self.ids.get(&answer) {
            return *id;
        }
        let id = u32::try_from(self.list.len()).unwrap();
        self.ids.insert(answer.clone(), id);
        self.list.push(answer);
        id
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// The primary: writer and observer
// ---------------------------------------------------------------------------

struct Statement {
    iteration: usize,
    label: &'static str,
    sql: String,
    /// The primary's sequence right before and right after the statement.
    before: u64,
    after: u64,
    /// When the writer sent it and when its answer came back (ms since the
    /// readers started): the ends of the detector's windows.
    sent_ms: u64,
    done_ms: u64,
}

/// What the primary passed through.
struct Model {
    queries: Vec<Query>,
    answers: Interner,
    /// `states[k][q]`: the primary's answer to query `q` in state `k` — 0
    /// before the first statement, `k` after `statements[k - 1]`.
    states: Vec<Vec<u32>>,
    statements: Vec<Statement>,
}

impl Model {
    fn answer(&self, id: u32) -> &Answer {
        &self.answers.list[id as usize]
    }

    fn state_name(&self, k: usize) -> String {
        match k.checked_sub(1).and_then(|index| self.statements.get(index)) {
            None => "state 0 (before the first statement)".to_string(),
            Some(statement) => format!(
                "state {k} (iteration {}, after {})",
                statement.iteration, statement.label
            ),
        }
    }
}

/// The primary as every session but the writer's sees it: every reader query.
async fn observe(observer: &Client, queries: &[Query], answers: &mut Interner, asked: &mut u64) -> Vec<u32> {
    let mut state = Vec::with_capacity(queries.len());
    for query in queries {
        *asked += 1;
        let answer = ask(observer, &tagged(&query.sql, "observer", *asked)).await;
        assert!(
            !matches!(answer, Answer::Broken(_)),
            "the primary's observer could not ask `{}`: {}",
            query.sql,
            answer.brief()
        );
        state.push(answers.intern(answer));
    }
    state
}

/// The observer and the cache-bypassing readers add a trailing comment to
/// every query: it must not change an answer.
async fn preflight(observer: &Client, queries: &[Query]) {
    for query in queries {
        let plain = ask(observer, &query.sql).await;
        let commented = ask(observer, &tagged(&query.sql, "preflight", 0)).await;
        assert_eq!(
            plain, commented,
            "a trailing comment changes the primary's answer to `{}`: the readers would not ask what the observer \
             records",
            query.sql
        );
    }
}

/// Run `iterations` of [`script`] on the primary, recording every state.
async fn drive_primary(
    primary: &Daemon,
    writer: &Client,
    observer: &Client,
    iterations: usize,
    started: Instant,
) -> Model {
    let queries = queries();
    let mut answers = Interner::default();
    let mut asked = 0;
    let mut states = vec![observe(observer, &queries, &mut answers, &mut asked).await];
    let mut statements = Vec::new();
    for iteration in 1..=iterations {
        for step in script(iteration) {
            let before = primary_sequence(observer).await;
            let sent_ms = elapsed_ms(started);
            if let Err(error) = writer.batch_execute(&step.sql).await {
                panic!(
                    "primary, iteration {iteration}, {}: `{}` failed: {error} ({}); log tail: {}",
                    step.label,
                    step.sql,
                    message(&error),
                    primary.tail(12)
                );
            }
            let done_ms = elapsed_ms(started);
            let after = primary_sequence(observer).await;
            if iteration == 1 && step.label == "insert_rows" {
                preflight(observer, &queries).await;
            }
            states.push(observe(observer, &queries, &mut answers, &mut asked).await);
            statements.push(Statement {
                iteration,
                label: step.label,
                sql: step.sql,
                before,
                after,
                sent_ms,
                done_ms,
            });
        }
    }
    Model {
        queries,
        answers,
        states,
        statements,
    }
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// A unique trailing comment on every query: no result or plan cache answers it.
    Fresh,
    /// The same text every time, as an application asks: the caches answer it.
    Cached,
}

/// One answer a reader got.
struct Observation {
    query: usize,
    /// The reader's own answer id (translated into the model's to validate).
    answer: u32,
    /// The table round it belongs to: a full scan, the table's other
    /// queries, the full scan again.
    segment: u32,
    /// When the query was sent and when its answer came (ms since the
    /// readers started).
    started_ms: u64,
    at_ms: u64,
    /// On the standby: its replay position when the round started.
    floor: Option<u64>,
}

struct ReaderLog {
    name: String,
    standby: bool,
    flavor: Flavor,
    answers: Interner,
    observations: Vec<Observation>,
    rounds: u64,
}

/// Ask [`round_plan`] in a loop until `stop`; a broken connection ends it.
#[allow(clippy::too_many_arguments)]
async fn reader(
    name: String,
    port: u16,
    standby: bool,
    flavor: Flavor,
    sqls: Arc<Vec<String>>,
    plan: Arc<Vec<Vec<usize>>>,
    stop: Arc<AtomicBool>,
    started: Instant,
) -> ReaderLog {
    let client = connect(port).await;
    let mut log = ReaderLog {
        name,
        standby,
        flavor,
        answers: Interner::default(),
        observations: Vec::new(),
        rounds: 0,
    };
    let mut segment = 0_u32;
    let mut asked = 0_u64;
    'rounds: while !stop.load(Ordering::Acquire) {
        log.rounds += 1;
        for order in plan.iter() {
            segment += 1;
            let floor = if standby {
                replay_position(&ask(&client, REPLAY_POSITION_SQL).await)
            } else {
                None
            };
            for &query in order {
                asked += 1;
                let sql = match flavor {
                    Flavor::Fresh => tagged(&sqls[query], &log.name, asked),
                    Flavor::Cached => sqls[query].clone(),
                };
                let started_ms = elapsed_ms(started);
                let answer = ask(&client, &sql).await;
                let broken = matches!(answer, Answer::Broken(_));
                let answer = log.answers.intern(answer);
                log.observations.push(Observation {
                    query,
                    answer,
                    segment,
                    started_ms,
                    at_ms: elapsed_ms(started),
                    floor,
                });
                if broken {
                    break 'rounds;
                }
            }
        }
    }
    log
}

/// The standby's replay position over time `(ms, sequence)`, for the report.
async fn monitor(port: u16, stop: Arc<AtomicBool>, started: Instant) -> Vec<(u64, u64)> {
    let client = connect(port).await;
    let mut samples = Vec::new();
    while !stop.load(Ordering::Acquire) {
        if let Some(position) = replay_position(&ask(&client, REPLAY_POSITION_SQL).await) {
            samples.push((elapsed_ms(started), position));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    samples
}

/// Wait until the standby replayed through `through`.
async fn wait_replayed(port: u16, through: u64, budget: Duration, standby: &Daemon) {
    let client = connect(port).await;
    let deadline = Instant::now() + budget;
    loop {
        let replayed = value(&client, REPLAY_POSITION_SQL).await.map(|text| lsn(&text));
        if replayed.is_some_and(|replayed| replayed >= through) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the standby never replayed through {through} (at {replayed:?}); log tail: {}",
            standby.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// The detector
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Torn,
    Window,
    Pair,
    Broken,
    NotReady,
}

#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    observations: usize,
    torn: usize,
    window: usize,
    pair: usize,
    broken: usize,
    not_ready: usize,
}

impl Tally {
    fn count(&mut self, kind: Kind) {
        match kind {
            Kind::Torn => self.torn += 1,
            Kind::Window => self.window += 1,
            Kind::Pair => self.pair += 1,
            Kind::Broken => self.broken += 1,
            Kind::NotReady => self.not_ready += 1,
        }
    }
}

struct Violation {
    reader: usize,
    kind: Kind,
    query: usize,
    /// The model's answer id.
    answer: u32,
    at_ms: u64,
    /// The states the answer may legitimately show ([`window`]).
    window: (usize, usize),
    /// The states this answer matches.
    matching: Rc<[usize]>,
    /// For a pair violation, the states its full scans pin.
    span: Option<(usize, usize)>,
}

/// The states `observation` may legitimately show, from clocks and
/// positions (module documentation, "The detector"): at most the statements
/// sent before its answer arrived; at least, on the primary, those answered
/// before it was sent, and on the standby, for a cache-bypassing reader,
/// those the standby had applied when the round started.
fn window(model: &Model, log: &ReaderLog, observation: &Observation) -> (usize, usize) {
    let statements = &model.statements;
    let high = statements.partition_point(|statement| statement.sent_ms <= observation.at_ms);
    let low = match (log.standby, log.flavor) {
        (false, _) => statements.partition_point(|statement| statement.done_ms < observation.started_ms),
        (true, Flavor::Fresh) => observation.floor.map_or(0, |floor| {
            statements.partition_point(|statement| statement.after <= floor)
        }),
        (true, Flavor::Cached) => 0,
    };
    (low.min(high), high)
}

/// Whether `answer` is what the primary answered `query` in state `k`; for a
/// kNN query, also whether an approximate index could give it over that
/// state: a non-empty subset of the state's rows, at most the limit, where
/// the primary's own answer is non-empty.
fn valid(model: &Model, query: usize, answer: u32, k: usize) -> bool {
    let recorded = model.states[k][query];
    if recorded == answer {
        return true;
    }
    let spec = &model.queries[query];
    if !spec.knn {
        return false;
    }
    let (
        Answer::Rows { columns, rows },
        Answer::Rows {
            columns: expected,
            rows: exact,
        },
        Answer::Rows {
            columns: all_columns,
            rows: all_rows,
        },
    ) = (
        model.answer(answer),
        model.answer(recorded),
        model.answer(model.states[k][spec.all]),
    )
    else {
        return false;
    };
    let position = |name: &str| all_columns.iter().position(|column| column == name);
    let (Some(id), Some(it)) = (position("id"), position("it")) else {
        return false;
    };
    columns == expected
        && !rows.is_empty()
        && !exact.is_empty()
        && rows.len() <= KNN_LIMIT
        && rows.iter().all(|row| {
            row.len() == 2
                && all_rows
                    .iter()
                    .any(|full| full.get(id) == row.first() && full.get(it) == row.get(1))
        })
}

/// The states each (query, answer) matches, computed once.
struct Matcher<'m> {
    model: &'m Model,
    cache: HashMap<(usize, u32), Rc<[usize]>>,
}

impl Matcher<'_> {
    fn states(&mut self, query: usize, answer: u32) -> Rc<[usize]> {
        let model = self.model;
        let matching = self.cache.entry((query, answer)).or_insert_with(|| {
            (0..model.states.len())
                .filter(|&k| valid(model, query, answer, k))
                .collect::<Vec<_>>()
                .into()
        });
        Rc::clone(matching)
    }
}

/// Validate one reader's session (see the module documentation). `ids` maps
/// the reader's answer ids to the model's.
fn check(
    reader: usize,
    log: &ReaderLog,
    ids: &[u32],
    matcher: &mut Matcher<'_>,
    violations: &mut Vec<Violation>,
) -> Tally {
    let mut tally = Tally::default();
    let observations = &log.observations;
    let mut start = 0;
    while start < observations.len() {
        let segment = observations[start].segment;
        let end = start
            + observations[start..]
                .iter()
                .take_while(|observation| observation.segment == segment)
                .count();
        let round = &observations[start..end];
        for observation in round {
            tally.observations += 1;
            let answer = ids[observation.answer as usize];
            let content = matcher.model.answer(answer);
            let matching = matcher.states(observation.query, answer);
            let (low, high) = window(matcher.model, log, observation);
            let kind = if matches!(content, Answer::Broken(_)) {
                Some(Kind::Broken)
            } else if matching.is_empty() && content.not_ready() {
                Some(Kind::NotReady)
            } else if matching.is_empty() {
                Some(Kind::Torn)
            } else if !matching.iter().any(|k| (low..=high).contains(k)) {
                Some(Kind::Window)
            } else {
                None
            };
            if let Some(kind) = kind {
                tally.count(kind);
                violations.push(Violation {
                    reader,
                    kind,
                    query: observation.query,
                    answer,
                    at_ms: observation.at_ms,
                    window: (low, high),
                    matching,
                    span: None,
                });
            }
        }

        // The pair check: the round's two full scans agree and pin a
        // contiguous run of states; every query between them must match one.
        if let (Some(first), Some(last)) = (round.first(), round.last()) {
            let scan = ids[first.answer as usize];
            if round.len() >= 3
                && first.query == last.query
                && scan == ids[last.answer as usize]
                && matches!(matcher.model.answer(scan), Answer::Rows { .. })
            {
                let span = matcher.states(first.query, scan);
                if let (Some(&low), Some(&high)) = (span.first(), span.last()) {
                    if high - low + 1 == span.len() {
                        for middle in &round[1..round.len() - 1] {
                            let answer = ids[middle.answer as usize];
                            let matching = matcher.states(middle.query, answer);
                            if !matching.is_empty() && !matching.iter().any(|k| (low..=high).contains(k)) {
                                tally.count(Kind::Pair);
                                violations.push(Violation {
                                    reader,
                                    kind: Kind::Pair,
                                    query: middle.query,
                                    answer,
                                    at_ms: middle.at_ms,
                                    window: window(matcher.model, log, middle),
                                    matching,
                                    span: Some((low, high)),
                                });
                            }
                        }
                    }
                }
            }
        }
        start = end;
    }
    tally
}

fn format_lsn(sequence: u64) -> String {
    format!("{:X}/{:X}", sequence >> 32, sequence & 0xFFFF_FFFF)
}

/// The statements the standby was applying around `at_ms` (from the replay
/// positions sampled before and after it).
fn in_flight(model: &Model, samples: &[(u64, u64)], at_ms: u64) -> String {
    let index = samples.partition_point(|(at, _)| *at <= at_ms);
    let low = index
        .checked_sub(2)
        .and_then(|before| samples.get(before))
        .or_else(|| samples.first());
    let high = samples.get(index).or_else(|| samples.last());
    let (Some(&(_, low)), Some(&(_, high))) = (low, high) else {
        return "the standby's replay position is unknown".to_string();
    };
    let (low, high) = (low.min(high), low.max(high));
    let statements: Vec<String> = model
        .statements
        .iter()
        .enumerate()
        .filter(|(_, statement)| statement.after > low && statement.before <= high)
        .take(6)
        .map(|(index, statement)| format!("[{}] {} #{}", index + 1, statement.label, statement.iteration))
        .collect();
    format!(
        "the standby replayed {}..{} around then: {}",
        format_lsn(low),
        format_lsn(high),
        if statements.is_empty() {
            "between statements".to_string()
        } else {
            statements.join(", ")
        }
    )
}

// ---------------------------------------------------------------------------
// The census: write groups per statement, from the primary's archive
// ---------------------------------------------------------------------------

struct Group {
    first: u64,
    last: u64,
    records: usize,
    /// Key description → records.
    keys: BTreeMap<String, usize>,
}

/// A key's class, with its table — or, for a catalog key, the key itself.
fn describe_key(key: &[u8]) -> String {
    let classified = classify(key);
    let named = matches!(
        classified.class,
        KeyClass::MetaTable
            | KeyClass::MetaIndex
            | KeyClass::TableConstraints
            | KeyClass::Statistics
            | KeyClass::IdxSnap
            | KeyClass::IdxSnapMarker
            | KeyClass::VecSnap
            | KeyClass::VecSnapMarker
            | KeyClass::Unknown
    );
    match classified.table {
        Some(table) => format!("{:?}({table})", classified.class),
        None if named => format!(
            "{:?}[{}]",
            classified.class,
            String::from_utf8_lossy(key.get(..key.len().min(48)).unwrap_or(key))
        ),
        None => format!("{:?}", classified.class),
    }
}

/// Every write group the primary archived, in order.
fn archived_groups(archive: &Path) -> Vec<Group> {
    let mut groups = Vec::new();
    for segment in sealed(archive) {
        for batch in segment.frames(None).unwrap() {
            let batch = batch.unwrap();
            let ops = decode_raw_batch(batch.data()).unwrap();
            let mut keys = BTreeMap::new();
            for op in &ops {
                let name = match op {
                    RawOp::Put { key, .. } => describe_key(key),
                    RawOp::Delete { key } => format!("delete {}", describe_key(key)),
                    RawOp::DeleteRange { start, .. } => format!("range delete from {}", describe_key(start)),
                };
                *keys.entry(name).or_insert(0_usize) += 1;
            }
            groups.push(Group {
                first: batch.first_sequence(),
                last: batch.last_sequence(),
                records: ops.len(),
                keys,
            });
        }
    }
    groups
}

fn describe_keys(keys: &BTreeMap<String, usize>) -> String {
    let parts: Vec<String> = keys.iter().map(|(name, count)| format!("{name} x{count}")).collect();
    parts.join(", ")
}

/// Attribute every archived group to the statement whose sequence interval
/// holds it, log the census (`CENSUS` lines) and return the statements that
/// produced more than one write group in some iteration.
fn log_census(model: &Model, groups: &[Group]) -> Vec<&'static str> {
    let mut per_statement: Vec<Vec<&Group>> = vec![Vec::new(); model.statements.len()];
    let mut between: Vec<&Group> = Vec::new();
    for group in groups {
        let index = model
            .statements
            .partition_point(|statement| statement.after < group.last);
        match model.statements.get(index) {
            Some(statement) if statement.before < group.first => per_statement[index].push(group),
            _ => between.push(group),
        }
    }
    let iterations = model.statements.last().map_or(0, |statement| statement.iteration);
    eprintln!(
        "CENSUS write groups per statement on the primary ({iterations} iterations; {} archived groups, attributed \
         by sequence interval)",
        groups.len()
    );
    let mut several = Vec::new();
    for step in script(1) {
        let mut distribution: BTreeMap<usize, usize> = BTreeMap::new();
        for (statement, groups) in model.statements.iter().zip(&per_statement) {
            if statement.label == step.label {
                *distribution.entry(groups.len()).or_insert(0) += 1;
            }
        }
        let counts: Vec<String> = distribution
            .iter()
            .map(|(groups, times)| format!("{groups} group(s) x{times}"))
            .collect();
        let multi = distribution.keys().any(|groups| *groups > 1);
        if multi {
            several.push(step.label);
        }
        eprintln!(
            "CENSUS   {:<24} {}{}",
            step.label,
            counts.join(", "),
            if multi { "   <- more than one write group" } else { "" }
        );
    }
    eprintln!("CENSUS first iteration, group by group:");
    for (index, (statement, groups)) in model.statements.iter().zip(&per_statement).enumerate() {
        if statement.iteration != 1 {
            continue;
        }
        eprintln!(
            "CENSUS   [{}] {}: {}",
            index + 1,
            statement.label,
            abbreviate(&statement.sql, 110)
        );
        for group in groups {
            eprintln!(
                "CENSUS     {}..{} ({} records): {}",
                group.first,
                group.last,
                group.records,
                describe_keys(&group.keys)
            );
        }
    }
    eprintln!(
        "CENSUS groups in no statement's interval (between statements): {}",
        between.len()
    );
    for group in between.iter().take(12) {
        eprintln!(
            "CENSUS     {}..{} ({} records): {}",
            group.first,
            group.last,
            group.records,
            describe_keys(&group.keys)
        );
    }
    several
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

struct Run {
    name: &'static str,
    iterations: usize,
    /// Environment of the standby daemon (the applier's test hooks).
    standby_env: Vec<(&'static str, String)>,
    /// Also run the reader on the primary.
    primary_control: bool,
    census: bool,
    catch_up: Duration,
}

struct Outcome {
    name: &'static str,
    model: Model,
    logs: Vec<ReaderLog>,
    tallies: Vec<Tally>,
    violations: Vec<Violation>,
    samples: Vec<(u64, u64)>,
    multi_group: Option<Vec<&'static str>>,
    standby_log: String,
    primary_log: String,
    standby_ready: Option<u16>,
    seconds: f64,
}

async fn run(options: Run) -> Outcome {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(PRIMARY_CONFIG),
    );
    let writer = connect(pair.pg_p).await;
    let observer = connect(pair.pg_p).await;
    let standby = Daemon::spawn_with(
        &dir,
        "standby",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        None,
        &SpawnOptions {
            listen: None,
            env: options.standby_env.clone(),
        },
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;

    let started = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_queries = queries();
    let sqls = Arc::new(reader_queries.iter().map(|query| query.sql.clone()).collect::<Vec<_>>());
    let plan = Arc::new(round_plan(&reader_queries));
    let mut readers = Vec::new();
    let mut roles = vec![
        (true, Flavor::Fresh),
        (true, Flavor::Fresh),
        (true, Flavor::Fresh),
        (true, Flavor::Cached),
    ];
    if options.primary_control {
        roles.push((false, Flavor::Fresh));
    }
    for (index, (on_standby, flavor)) in roles.into_iter().enumerate() {
        let name = format!(
            "{}-{}-{index}",
            if on_standby { "standby" } else { "primary" },
            match flavor {
                Flavor::Fresh => "fresh",
                Flavor::Cached => "cached",
            }
        );
        let port = if on_standby { pair.pg_s } else { pair.pg_p };
        readers.push(tokio::spawn(reader(
            name,
            port,
            on_standby,
            flavor,
            Arc::clone(&sqls),
            Arc::clone(&plan),
            Arc::clone(&stop),
            started,
        )));
    }
    let positions = tokio::spawn(monitor(pair.pg_s, Arc::clone(&stop), started));

    let mut model = drive_primary(&primary, &writer, &observer, options.iterations, started).await;
    let through = model.statements.last().map_or(0, |statement| statement.after);
    wait_replayed(pair.pg_s, through, options.catch_up, &standby).await;
    // Let the readers see the final state too.
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.store(true, Ordering::Release);
    let mut logs = Vec::new();
    for handle in readers {
        logs.push(handle.await.unwrap());
    }
    let samples = positions.await.unwrap();
    let seconds = started.elapsed().as_secs_f64();
    let standby_ready = http(pair.http_s, "/ready").await.map(|(code, _)| code);

    let multi_group = if options.census {
        let archive = dir.join("primary.archive");
        wait_sealed_through(&archive, through, &primary).await;
        Some(log_census(&model, &archived_groups(&archive)))
    } else {
        None
    };

    // Validate (no await below: the matcher's sets are `Rc`).
    let mut ids: Vec<Vec<u32>> = Vec::new();
    for log in &logs {
        let mut map = Vec::with_capacity(log.answers.list.len());
        for answer in &log.answers.list {
            map.push(model.answers.intern(answer.clone()));
        }
        ids.push(map);
    }
    let mut violations = Vec::new();
    let mut tallies = Vec::new();
    {
        let mut matcher = Matcher {
            model: &model,
            cache: HashMap::new(),
        };
        for (index, log) in logs.iter().enumerate() {
            tallies.push(check(index, log, &ids[index], &mut matcher, &mut violations));
        }
    }
    let outcome = Outcome {
        name: options.name,
        model,
        logs,
        tallies,
        violations,
        samples,
        multi_group,
        standby_log: standby.log(),
        primary_log: primary.log(),
        standby_ready,
        seconds,
    };
    eprintln!("{}", outcome.summary());
    outcome
}

fn last_lines(log: &str, lines: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join(" | ")
}

impl Outcome {
    fn describe(&self, violation: &Violation) -> String {
        let model = &self.model;
        let query = &model.queries[violation.query];
        let (low, high) = violation.window;
        let mut text = format!(
            "{} {:?} at {} ms: {} {} `{}` -> {}; its window {} ..= {}",
            self.logs[violation.reader].name,
            violation.kind,
            violation.at_ms,
            query.table,
            query.name,
            query.sql,
            model.answer(violation.answer).brief(),
            model.state_name(low),
            model.state_name(high)
        );
        if let Some((pinned_low, pinned_high)) = violation.span {
            let _ = write!(
                text,
                "; the full scans around it pin {} ..= {}",
                model.state_name(pinned_low),
                model.state_name(pinned_high)
            );
        }
        match (violation.matching.first(), violation.matching.last()) {
            (Some(&first), Some(&last)) => {
                let _ = write!(
                    text,
                    "; the answer matches {} state(s), {} ..= {}",
                    violation.matching.len(),
                    model.state_name(first),
                    model.state_name(last)
                );
            }
            _ => {
                // What the primary answered inside the window.
                let mut seen: Vec<u32> = Vec::new();
                for k in low..=high.min(model.states.len().saturating_sub(1)) {
                    let id = model.states[k][violation.query];
                    if !seen.contains(&id) {
                        seen.push(id);
                    }
                }
                let shown: Vec<String> = seen.iter().take(3).map(|id| model.answer(*id).brief()).collect();
                let _ = write!(text, "; the primary answered, inside the window: {}", shown.join(" | "));
            }
        }
        let _ = write!(text, "; {}", in_flight(model, &self.samples, violation.at_ms));
        text
    }

    fn summary(&self) -> String {
        let mut text = format!(
            "DDL-WINDOW [{}] {} iteration(s), {} statements, {} primary states, {} distinct answers, {:.1} s\n",
            self.name,
            self.model.statements.last().map_or(0, |statement| statement.iteration),
            self.model.statements.len(),
            self.model.states.len(),
            self.model.answers.list.len(),
            self.seconds
        );
        if let Some(multi) = &self.multi_group {
            let _ = writeln!(
                text,
                "DDL-WINDOW   statements that produced more than one write group: {}",
                if multi.is_empty() {
                    "none".to_string()
                } else {
                    multi.join(", ")
                }
            );
        }
        for (index, (log, tally)) in self.logs.iter().zip(&self.tallies).enumerate() {
            let _ = writeln!(
                text,
                "DDL-WINDOW   {:<18} {} rounds, {} answers: torn {}, window {}, pair {}, broken {}, not ready {}{}",
                log.name,
                log.rounds,
                tally.observations,
                tally.torn,
                tally.window,
                tally.pair,
                tally.broken,
                tally.not_ready,
                if log.standby { "" } else { "   (control on the primary)" }
            );
            for violation in self
                .violations
                .iter()
                .filter(|violation| violation.reader == index)
                .take(SHOWN_PER_READER)
            {
                let _ = writeln!(text, "DDL-WINDOW     {}", self.describe(violation));
            }
        }
        let _ = write!(text, "DDL-WINDOW   standby /ready at the end: {:?}", self.standby_ready);
        text
    }

    /// R5's acceptance (spec R2, "Decision 2026-10-31: no cut markers"): on
    /// every reader, the standby's and the primary control alike, torn,
    /// window, pair, broken and not ready are all zero. For a run whose
    /// standby applies the primary's write groups as they are (no hook breaks
    /// them).
    fn assert_clean(&self) {
        let mut failures = Vec::new();
        if !self.logs.iter().any(|log| !log.standby) {
            failures.push("no primary control reader ran".to_string());
        }
        for (log, tally) in self.logs.iter().zip(&self.tallies) {
            if tally.observations == 0 {
                failures.push(format!("{} asked nothing", log.name));
            }
            if tally.torn + tally.window + tally.pair + tally.broken + tally.not_ready > 0 {
                failures.push(format!(
                    "{}: torn {}, window {}, pair {}, broken {}, not ready {}",
                    log.name, tally.torn, tally.window, tally.pair, tally.broken, tally.not_ready
                ));
            }
        }
        for (daemon, log) in [("standby", &self.standby_log), ("primary", &self.primary_log)] {
            if log.contains("panicked at") {
                let panics: Vec<&str> = log
                    .lines()
                    .filter(|line| line.contains("panicked at"))
                    .take(3)
                    .collect();
                failures.push(format!("the {daemon} panicked: {}", panics.join(" | ")));
            }
        }
        if self.standby_ready != Some(200) {
            failures.push(format!(
                "the standby is not ready at the end (/ready {:?}); log tail: {}",
                self.standby_ready,
                last_lines(&self.standby_log, 12)
            ));
        }
        assert!(
            failures.is_empty(),
            "a reader saw answers that match no state the primary passed through at a statement boundary, or none \
             in their window (plan decision 7, spec R2; R5 owns the fix): {}\n{}",
            failures.join("; "),
            self.summary()
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Spec R2: the plain run, with the census. R5's acceptance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "R5: statement atomicity on the primary (docs/plans/specs/R5_statement_atomicity.md)"]
async fn standby_readers_never_see_a_torn_state_while_the_primary_runs_ddl() {
    let iterations = std::env::var(ITERATIONS_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(ITERATIONS);
    let outcome = run(Run {
        name: "plain",
        iterations,
        standby_env: Vec::new(),
        primary_control: true,
        census: true,
        catch_up: Duration::from_secs(120),
    })
    .await;
    outcome.assert_clean();
}

/// Spec R2 with decision 7's per-batch publication made the worst case: the
/// standby's applier publishes every write group on its own and holds it
/// open for [`PACE_MS`], so every state between two groups of a multi-group
/// statement is served for that long. R5's acceptance, like the plain run.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "R5: statement atomicity on the primary (docs/plans/specs/R5_statement_atomicity.md)"]
async fn standby_readers_never_see_a_torn_state_when_every_write_group_is_published_on_its_own() {
    let outcome = run(Run {
        name: "paced",
        iterations: PACED_ITERATIONS,
        standby_env: vec![(APPLY_PACE_ENV, PACE_MS.to_string())],
        primary_control: true,
        census: false,
        catch_up: Duration::from_secs(300),
    })
    .await;
    assert!(
        outcome.standby_log.contains("test hooks armed"),
        "the standby's applier did not arm {APPLY_PACE_ENV} (a binary without internal-tests?); log tail: {}",
        last_lines(&outcome.standby_log, 12)
    );
    outcome.assert_clean();
}

/// The detector's self-check (spec R2: "make it fail on purpose first"): the
/// standby's applier writes every write group carrying two or more `ddl_a`
/// rows in two halves and admits readers between them for
/// [`SPLIT_PAUSE_MS`] — half of one primary write group, a state the primary
/// never passed through. The detector must report it as torn. No primary
/// control reader: nothing is asserted about the primary here, and that reader
/// can trigger finding (b)'s race (`tests/physical_ddl_window_repro.rs`), which
/// would fail this run for a reason that is not the detector's.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_detector_reports_a_write_group_split_by_the_test_hook() {
    let outcome = run(Run {
        name: "split self-check",
        iterations: SPLIT_ITERATIONS,
        standby_env: vec![(SPLIT_GROUP_ENV, format!("ddl_a:{SPLIT_PAUSE_MS}"))],
        primary_control: false,
        census: false,
        catch_up: Duration::from_secs(180),
    })
    .await;
    let splits = outcome.standby_log.matches("applied in two writes").count();
    assert!(
        splits > 0,
        "the hook split no write group, so there was nothing to detect ({SPLIT_GROUP_ENV} not armed, or no group \
         carried two ddl_a rows); log tail: {}",
        last_lines(&outcome.standby_log, 12)
    );
    let torn: Vec<&Violation> = outcome
        .violations
        .iter()
        .filter(|violation| {
            outcome.logs[violation.reader].standby
                && violation.kind == Kind::Torn
                && outcome.model.queries[violation.query].table == "ddl_a"
        })
        .collect();
    assert!(
        !torn.is_empty(),
        "the detector reported no torn read of ddl_a while {splits} write group(s) were served half-applied for \
         {SPLIT_PAUSE_MS} ms each: it is vacuous\n{}",
        outcome.summary()
    );
    eprintln!(
        "DDL-WINDOW self-check: {splits} write group(s) split, {} torn answer(s) of ddl_a reported, e.g. {}",
        torn.len(),
        outcome.describe(torn[0])
    );
}
