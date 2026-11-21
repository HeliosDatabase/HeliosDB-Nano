#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! SQL Logic Test runner.
//!
//! Ordinary tests: the reader, the value rules, the per-file worker processes, the
//! baseline check, and a small corpus under `tests/fixtures/slt/` run through the
//! embedded API, the PostgreSQL wire and the MySQL wire.
//!
//! `the_public_corpus` runs the public SQL Logic Test corpus, which is not in the
//! repository: `scripts/slt_fetch_corpus.sh` downloads it at a pinned revision and
//! `scripts/slt_run.sh` runs it and writes `docs/correctness/sqllogictest.*`.

#[path = "slt_support/engines.rs"]
mod engines;
#[path = "slt_support/run.rs"]
mod run;
#[path = "slt_support/script.rs"]
mod script;
#[path = "slt_support/supervise.rs"]
mod supervise;

use engines::{Engine, Interface};
use run::{check_baseline, raise_baseline, render_page, Baseline, Counts, FileOutcome, Incident, RunResult};
use script::{compare, hash_values, parse, render, render_rows, Cell, Kind, Mismatch, SortMode};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use supervise::{run_file, Limits, WORKER_ENV};

/// The worker role of the per-file process isolation. Without the environment variable
/// this is an empty test.
#[test]
fn slt_worker() {
    if let Ok(spec) = std::env::var(WORKER_ENV) {
        supervise::worker_main(&spec);
    }
}

// ---------------------------------------------------------------------------
// The reader and the value rules
// ---------------------------------------------------------------------------

const SAMPLE: &str = "\
# a comment
hash-threshold 4

statement ok
CREATE TABLE t(a INTEGER,
               b TEXT)

statement error
INSERT INTO nosuch VALUES(1)

skipif mysql # trailing words are ignored
onlyif postgresql
query IT rowsort label-1
SELECT a, b
  FROM t
----
1
one
2
(empty)

query I nosort
SELECT a FROM t WHERE a > 100

onlyif mssql
halt

query III valuesort
SELECT 1, 2, 3 FROM t
----
6 values hashing to 0123456789abcdef0123456789abcdef
";

#[test]
fn the_reader_takes_every_record_form() {
    let records = parse(SAMPLE).unwrap();
    assert_eq!(records.len(), 7);
    assert_eq!(records[0].kind, Kind::HashThreshold(4));
    assert_eq!(records[1].kind, Kind::Statement { expect_ok: true });
    assert_eq!(records[1].sql, "CREATE TABLE t(a INTEGER,\n               b TEXT)");
    assert_eq!(records[1].line, 4);
    assert_eq!(records[2].kind, Kind::Statement { expect_ok: false });
    match &records[3].kind {
        Kind::Query {
            types,
            sort,
            label,
            expected,
        } => {
            assert_eq!(types, "IT");
            assert_eq!(*sort, SortMode::RowSort);
            assert_eq!(label.as_deref(), Some("label-1"));
            assert_eq!(expected, &["1", "one", "2", "(empty)"]);
        }
        other => panic!("not a query: {other:?}"),
    }
    assert_eq!(records[3].sql, "SELECT a, b\n  FROM t");
    assert_eq!(
        records[3].line, 13,
        "the line of the record itself, after its conditions"
    );
    assert_eq!(records[3].conditions.len(), 2);
    assert!(records[3].applies_to("postgresql"));
    assert!(records[3].applies_to("PostgreSQL"), "engine names compare without case");
    assert!(!records[3].applies_to("mysql"));
    assert!(!records[3].applies_to("sqlite"), "onlyif another engine");
    match &records[4].kind {
        Kind::Query {
            expected, sort, label, ..
        } => {
            assert!(expected.is_empty(), "a query without a result block expects no rows");
            assert_eq!(*sort, SortMode::NoSort);
            assert!(label.is_none());
        }
        other => panic!("not a query: {other:?}"),
    }
    assert_eq!(records[5].kind, Kind::Halt);
    assert!(!records[5].applies_to("postgresql"));
    assert!(records[5].applies_to("mssql"));
    assert!(matches!(&records[6].kind, Kind::Query { sort: SortMode::ValueSort, expected, .. } if expected.len() == 1));
}

#[test]
fn the_reader_refuses_what_it_does_not_know() {
    assert!(parse("statement maybe\nSELECT 1\n").unwrap_err().contains("line 1"));
    assert!(parse("\n\nquery IX nosort\nSELECT 1\n").unwrap_err().contains("line 3"));
    assert!(parse("query I sideways\nSELECT 1\n").is_err());
    assert!(parse("frobnicate\n").is_err());
    assert!(parse("hash-threshold many\n").is_err());
    assert!(parse("# only a comment\n\n").unwrap().is_empty());
    assert_eq!(
        parse("statement ok\r\nSELECT 1\r\n\r\n").unwrap()[0].sql,
        "SELECT 1",
        "CRLF files"
    );
}

#[test]
fn values_are_rendered_by_the_column_type() {
    assert_eq!(render(&Cell::Null, 'I'), "NULL");
    assert_eq!(render(&Cell::Null, 'T'), "NULL");
    assert_eq!(render(&Cell::Int(-7), 'I'), "-7");
    assert_eq!(render(&Cell::Float(2.9), 'I'), "2", "cut towards zero");
    assert_eq!(render(&Cell::Float(-2.9), 'I'), "-2");
    assert_eq!(render(&Cell::Text("12".into()), 'I'), "12");
    assert_eq!(render(&Cell::Text("12.75".into()), 'I'), "12");
    assert_eq!(
        render(&Cell::Text("abc".into()), 'I'),
        "0",
        "a text with no number reads as 0"
    );
    assert_eq!(render(&Cell::Text("t".into()), 'I'), "1");
    assert_eq!(render(&Cell::Text("f".into()), 'I'), "0");
    assert_eq!(render(&Cell::Int(3), 'R'), "3.000");
    assert_eq!(render(&Cell::Float(1.0 / 3.0), 'R'), "0.333");
    assert_eq!(render(&Cell::Text("2.5000000000".into()), 'R'), "2.500");
    assert_eq!(render(&Cell::Text(String::new()), 'T'), "(empty)");
    assert_eq!(
        render(&Cell::Text("a\tb\u{e9}".into()), 'T'),
        "a@b@",
        "outside printable ASCII"
    );
    assert_eq!(render(&Cell::Int(5), 'T'), "5");
}

fn int_rows(rows: &[&[i64]]) -> Vec<Vec<Cell>> {
    rows.iter().map(|r| r.iter().map(|v| Cell::Int(*v)).collect()).collect()
}

#[test]
fn sorting_is_by_row_or_by_value_as_text() {
    let rows = int_rows(&[&[10, 2], &[9, 30], &[10, 1]]);
    assert_eq!(
        render_rows(&rows, "II", SortMode::NoSort).unwrap(),
        ["10", "2", "9", "30", "10", "1"]
    );
    // Text order, as the corpus was produced: "10" sorts before "9".
    assert_eq!(
        render_rows(&rows, "II", SortMode::RowSort).unwrap(),
        ["10", "1", "10", "2", "9", "30"]
    );
    assert_eq!(
        render_rows(&rows, "II", SortMode::ValueSort).unwrap(),
        ["1", "10", "10", "2", "30", "9"]
    );
    assert!(render_rows(&rows, "III", SortMode::NoSort)
        .unwrap_err()
        .contains("2 columns returned, 3 expected"));
}

#[test]
fn the_digest_is_the_corpus_digest() {
    // Known answers: MD5 of each value followed by a newline.
    assert_eq!(hash_values(&[]), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(hash_values(&["1".to_string()]), "b026324c6904b2a9cb4b88d6d61c81d1");
    let thirty: Vec<String> = (1..=30).map(|n| n.to_string()).collect();
    // `seq 1 30 | md5sum`
    assert_eq!(hash_values(&thirty), "da89770dab7dcab843f2ce109bb879ca");
}

#[test]
fn comparison_knows_values_digests_and_labels() {
    let mut labels = HashMap::new();
    let v = |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
    assert_eq!(compare(&v(&["1", "2"]), &v(&["1", "2"]), None, 8, &mut labels), Ok(()));
    assert_eq!(
        compare(&v(&["1", "3"]), &v(&["1", "2"]), None, 8, &mut labels),
        Err(Mismatch::Values {
            index: 1,
            expected: "2".into(),
            got: "3".into()
        })
    );
    assert_eq!(
        compare(&v(&["1"]), &v(&["1", "2"]), None, 8, &mut labels),
        Err(Mismatch::ValueCount { expected: 2, got: 1 })
    );
    // Over the threshold the script holds one digest line.
    let many = v(&["1", "2", "3"]);
    let line = format!("3 values hashing to {}", hash_values(&many));
    assert_eq!(compare(&many, &[line.clone()], None, 2, &mut labels), Ok(()));
    assert!(matches!(
        compare(&v(&["1", "2", "4"]), &[line.clone()], None, 2, &mut labels),
        Err(Mismatch::Hash { .. })
    ));
    assert_eq!(
        compare(&v(&["1", "2", "3", "4"]), &[line.clone()], None, 2, &mut labels),
        Err(Mismatch::ValueCount { expected: 3, got: 4 })
    );
    // A threshold of 0 never hashes.
    assert_eq!(compare(&many, &many, None, 0, &mut labels), Ok(()));
    // A result that came back smaller than the digest's count.
    assert_eq!(
        compare(&v(&["1"]), &[line], None, 2, &mut labels),
        Err(Mismatch::ValueCount { expected: 3, got: 1 })
    );
    // Two results under one label must be the same values.
    assert_eq!(compare(&v(&["1"]), &v(&["1"]), Some("x"), 8, &mut labels), Ok(()));
    assert_eq!(compare(&v(&["1"]), &v(&["1"]), Some("x"), 8, &mut labels), Ok(()));
    assert_eq!(
        compare(&v(&["2"]), &v(&["2"]), Some("x"), 8, &mut labels),
        Err(Mismatch::Label { label: "x".into() })
    );
}

/// An engine that answers from a table, to pin the counting without a database.
struct Canned {
    answers: HashMap<&'static str, Result<Vec<Vec<Cell>>, String>>,
    seen: Vec<String>,
}

impl Engine for Canned {
    fn statement(&mut self, sql: &str) -> Result<(), String> {
        self.query(sql).map(|_| ())
    }

    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        self.seen.push(sql.to_string());
        self.answers.get(sql).cloned().unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[test]
fn counting_and_failure_classes() {
    let text = "\
statement ok
A

statement ok
B

statement error
C

statement error
D

query I nosort
E
----
1

query I nosort
F
----
1

query II nosort
G
----
1
2

query I nosort
H
----
2
1

query I nosort
I

skipif postgresql
query I nosort
J
----
1

onlyif postgresql
halt

query I nosort
K
----
1
";
    let mut answers: HashMap<&'static str, Result<Vec<Vec<Cell>>, String>> = HashMap::new();
    answers.insert("B", Err("relation \"t17\" does not exist at position 42".to_string()));
    answers.insert("C", Err("no".to_string()));
    answers.insert("E", Ok(int_rows(&[&[1]])));
    answers.insert("F", Ok(int_rows(&[&[5]])));
    answers.insert("G", Ok(int_rows(&[&[1]])));
    answers.insert("H", Ok(int_rows(&[&[1], &[2]])));
    answers.insert("I", Err("boom".to_string()));
    let mut engine = Canned {
        answers,
        seen: Vec::new(),
    };
    let records = parse(text).unwrap();
    let mut lines = Vec::new();
    let out = run::run_script(
        &mut engine,
        "postgresql",
        "f.test",
        &records,
        &HashMap::new(),
        &mut |l| lines.push(l),
    );
    assert_eq!(
        out.counts,
        Counts {
            passed: 3,
            failed: 6,
            skipped: 2,
            timed_out: 0,
            crashed: 0
        }
    );
    assert_eq!(
        engine.seen,
        ["A", "B", "C", "D", "E", "F", "G", "H", "I"],
        "J is skipped, K is after halt"
    );
    let classes: Vec<&str> = out.classes.keys().map(String::as_str).collect();
    assert_eq!(
        classes,
        [
            "query refused: boom",
            "statement accepted where the script expects an error",
            "statement refused: relation ? does not exist at position N",
            "wrong result: different values",
            "wrong result: number of columns",
            "wrong result: the right values in another order",
        ]
    );
    let refused = &out.classes["statement refused: relation ? does not exist at position N"];
    assert_eq!(
        (refused.count, refused.example.line, refused.example.sql.as_str()),
        (1, 4, "B")
    );

    // A record named in the skip list is counted where the list says and not run.
    let mut engine = Canned {
        answers: HashMap::new(),
        seen: Vec::new(),
    };
    let skip: HashMap<usize, String> = [(1, "timed_out".to_string()), (4, "crashed".to_string())].into();
    let out = run::run_script(&mut engine, "postgresql", "f.test", &records, &skip, &mut |_| {});
    assert_eq!((out.counts.timed_out, out.counts.crashed), (1, 1));
    assert!(!engine.seen.contains(&"A".to_string()) && !engine.seen.contains(&"B".to_string()));
}

// ---------------------------------------------------------------------------
// The result document, the page and the baseline
// ---------------------------------------------------------------------------

fn outcome(passed: u64, failed: u64) -> FileOutcome {
    let mut o = FileOutcome::default();
    o.counts.passed = passed;
    o.counts.failed = failed;
    if failed > 0 {
        o.classes.insert(
            "wrong result: different values".to_string(),
            run::FailureClass {
                count: failed,
                example: run::Example {
                    file: "x.test".into(),
                    line: 3,
                    sql: "SELECT 1".into(),
                    detail: "value 0: got 2, expected 1".into(),
                },
            },
        );
    }
    o
}

fn sample_result(a: u64, b: u64) -> RunResult {
    let mut files = BTreeMap::new();
    files.insert("select1.test".to_string(), outcome(a, 1));
    files.insert("index/in/10/slt_good_0.test".to_string(), outcome(b, 2));
    files.insert("index/in/10/slt_good_1.test".to_string(), outcome(1, 0));
    RunResult::build("embedded", "postgresql", "sample", "rev", &files, Vec::new())
}

#[test]
fn the_result_is_counted_per_directory_and_the_page_comes_from_it() {
    let r = sample_result(10, 20);
    assert_eq!(r.directories["."].passed, 10);
    assert_eq!(r.directories["index/in/10"].passed, 21);
    assert_eq!(r.totals.passed, 31);
    assert_eq!(r.totals.failed, 3);
    assert_eq!(r.failure_classes[0].1.count, 3, "one class across files adds up");
    let json = serde_json::to_string(&r).unwrap();
    let back: RunResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
    let page = render_page(&[back]);
    assert!(page.contains("| index/in/10 | 21 | 2 | 0 | 0 | 0 | 91.30% |"), "{page}");
    assert!(page.contains("| **all** | 31 | 3 | 0 | 0 | 0 | 91.18% |"), "{page}");
    assert!(
        page.contains("1. **wrong result: different values**: 3 records"),
        "{page}"
    );
    assert!(page.contains("Example: `x.test` line 3"), "{page}");
}

#[test]
fn the_baseline_check_fails_when_a_directory_loses_a_pass() {
    let mut baseline = Baseline::new();
    raise_baseline(&mut baseline, &sample_result(10, 20));
    assert_eq!(baseline["embedded"]["index/in/10"], 21);
    assert!(
        check_baseline(&baseline, &sample_result(10, 20)).is_empty(),
        "the same counts pass"
    );
    assert!(
        check_baseline(&baseline, &sample_result(11, 25)).is_empty(),
        "a gain passes"
    );
    let lost = check_baseline(&baseline, &sample_result(10, 19));
    assert_eq!(lost, ["embedded index/in/10: 20 passed, the baseline has 21"]);
    // A gain elsewhere does not pay for a loss.
    assert_eq!(check_baseline(&baseline, &sample_result(50, 19)).len(), 1);
    // Raising never lowers.
    raise_baseline(&mut baseline, &sample_result(5, 30));
    assert_eq!(
        (baseline["embedded"]["."], baseline["embedded"]["index/in/10"]),
        (10, 31)
    );
    // A directory in the baseline that the run does not have is a failure too.
    let mut other = sample_result(10, 31);
    other.directories.remove(".");
    assert_eq!(check_baseline(&baseline, &other).len(), 1);
    // Another interface has its own baseline.
    let mut wire = sample_result(0, 0);
    wire.interface = "pgwire".into();
    assert!(check_baseline(&baseline, &wire).is_empty());
}

// ---------------------------------------------------------------------------
// One process per file
// ---------------------------------------------------------------------------

fn limits(ms: u64) -> Limits {
    Limits {
        timeout: Duration::from_millis(ms),
        file_timeout: Duration::from_secs(120),
        max_incidents: 4,
    }
}

const SIX: &str = "\
statement ok
CREATE TABLE t(a INTEGER)

statement ok
INSERT INTO t VALUES(1)

statement ok
INSERT INTO t VALUES(2)

query I rowsort
SELECT a FROM t
----
1
2

statement ok
INSERT INTO t VALUES(3)

query I nosort
SELECT count(*) FROM t
----
3
";

#[test]
fn a_record_that_kills_the_worker_costs_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("six.test");
    std::fs::write(&file, SIX).unwrap();
    // Line 7 is the second INSERT: the worker dies there, is started again without it,
    // and the rest of the file still runs (and shows the row is missing).
    let (out, incidents) = run_file(
        &file,
        "six.test",
        Interface::Embedded,
        &limits(5_000),
        dir.path(),
        Some((7, "abort".to_string())),
    );
    assert_eq!(incidents.len(), 1, "{incidents:?}");
    assert_eq!((incidents[0].line, incidents[0].kind.as_str()), (7, "crashed"));
    assert_eq!(incidents[0].sql, "INSERT INTO t VALUES(2)");
    assert_eq!(
        out.counts,
        Counts {
            passed: 3,
            failed: 2,
            skipped: 0,
            timed_out: 0,
            crashed: 1
        },
        "{out:?}"
    );
}

#[test]
fn a_record_that_never_answers_times_out_and_the_file_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("six.test");
    std::fs::write(&file, SIX).unwrap();
    let (out, incidents) = run_file(
        &file,
        "six.test",
        Interface::Embedded,
        &limits(300),
        dir.path(),
        Some((16, "hang".to_string())),
    );
    assert_eq!(incidents.len(), 1, "{incidents:?}");
    assert_eq!((incidents[0].line, incidents[0].kind.as_str()), (16, "timed_out"));
    assert_eq!(
        out.counts,
        Counts {
            passed: 4,
            failed: 1,
            skipped: 0,
            timed_out: 1,
            crashed: 0
        },
        "{out:?}"
    );
}

#[test]
fn a_clean_file_needs_one_worker_and_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("six.test");
    std::fs::write(&file, SIX).unwrap();
    let (out, incidents) = run_file(&file, "six.test", Interface::Embedded, &limits(5_000), dir.path(), None);
    assert!(incidents.is_empty(), "{incidents:?}");
    assert_eq!((out.counts.passed, out.counts.failed), (6, 0), "{out:?}");
    let unreadable = dir.path().join("bad.test");
    std::fs::write(&unreadable, "frobnicate\n").unwrap();
    let (out, _) = run_file(
        &unreadable,
        "bad.test",
        Interface::Embedded,
        &limits(5_000),
        dir.path(),
        None,
    );
    assert_eq!(out.counts.failed, 1);
    assert!(out.classes.contains_key("script the runner cannot read"));
}

// ---------------------------------------------------------------------------
// Corpus runs
// ---------------------------------------------------------------------------

fn test_files(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "test") {
                let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((path, name));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// Run a list of files, each in its own worker process, `jobs` at a time.
fn run_corpus(
    files: &[(PathBuf, String)],
    interface: Interface,
    limits: &Limits,
    jobs: usize,
    scratch: &Path,
) -> (BTreeMap<String, FileOutcome>, Vec<Incident>) {
    let next = AtomicUsize::new(0);
    let done: Mutex<(BTreeMap<String, FileOutcome>, Vec<Incident>)> = Mutex::new((BTreeMap::new(), Vec::new()));
    std::thread::scope(|scope| {
        for job in 0..jobs.max(1) {
            let (next, done) = (&next, &done);
            let scratch = scratch.join(format!("job{job}"));
            std::fs::create_dir_all(&scratch).unwrap();
            scope.spawn(move || loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((path, name)) = files.get(at) else { break };
                let (outcome, mut incidents) = run_file(path, name, interface, limits, &scratch, None);
                let mut guard = done.lock().unwrap();
                guard.0.insert(name.clone(), outcome);
                guard.1.append(&mut incidents);
                let finished = guard.0.len();
                if finished % 25 == 0 || finished == files.len() {
                    eprintln!("slt {}: {finished}/{} files", interface.name(), files.len());
                }
            });
        }
    });
    let (outcomes, mut incidents) = done.into_inner().unwrap();
    incidents.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    (outcomes, incidents)
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slt")
}

fn fixture_baseline() -> Baseline {
    let text = std::fs::read_to_string(fixture_root().join("baseline.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The fixture corpus through one interface: no record may crash or time out, and no
/// directory may pass fewer records than `tests/fixtures/slt/baseline.json` holds. The
/// baseline is what the engine does today, not what it should do: the records that
/// fail are printed, and the baseline is raised when one of them is fixed
/// (`SLT_FIXTURE_BASELINE=update`).
fn fixture_run(interface: Interface) {
    let root = fixture_root();
    let files = test_files(&root);
    assert!(files.len() >= 5, "the fixture corpus is missing");
    let scratch = tempfile::tempdir().unwrap();
    let limits = Limits {
        timeout: Duration::from_secs(30),
        file_timeout: Duration::from_secs(300),
        max_incidents: 8,
    };
    let (outcomes, incidents) = run_corpus(&files, interface, &limits, 2, scratch.path());
    let result = RunResult::build(
        interface.name(),
        interface.engine_label(),
        "fixture",
        "",
        &outcomes,
        incidents,
    );
    for (class, f) in &result.failure_classes {
        eprintln!(
            "slt fixture {}: {} x {class} (e.g. {} line {}: {} | {})",
            interface.name(),
            f.count,
            f.example.file,
            f.example.line,
            f.example.sql.replace('\n', " "),
            f.example.detail
        );
    }
    eprintln!("slt fixture {}: {:?}", interface.name(), result.totals);
    assert!(
        result.incidents.is_empty(),
        "records crashed or timed out: {:#?}",
        result.incidents
    );
    assert!(
        result.totals.total() >= 400,
        "the fixture corpus shrank: {:?}",
        result.totals
    );
    if std::env::var("SLT_FIXTURE_BASELINE").as_deref() == Ok("update") {
        let path = root.join("baseline.json");
        let mut baseline: Baseline = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        raise_baseline(&mut baseline, &result);
        std::fs::write(&path, serde_json::to_string_pretty(&baseline).unwrap() + "\n").unwrap();
        return;
    }
    let baseline = fixture_baseline();
    assert!(
        baseline.contains_key(interface.name()),
        "no fixture baseline for {}",
        interface.name()
    );
    let problems = check_baseline(&baseline, &result);
    assert!(
        problems.is_empty(),
        "pass counts fell below the baseline: {problems:#?}"
    );
}

#[test]
fn the_fixture_corpus_through_the_embedded_api() {
    fixture_run(Interface::Embedded);
}

#[test]
fn the_fixture_corpus_through_the_postgresql_wire() {
    fixture_run(Interface::PgWire);
}

#[test]
fn the_fixture_corpus_through_the_mysql_wire() {
    fixture_run(Interface::MySqlWire);
}

/// The engine labels decide which records run: the MySQL wire runs the `onlyif mysql`
/// records and the other two interfaces skip them.
#[test]
fn engine_labels_follow_the_interface() {
    assert_eq!(Interface::Embedded.engine_label(), "postgresql");
    assert_eq!(Interface::PgWire.engine_label(), "postgresql");
    assert_eq!(Interface::MySqlWire.engine_label(), "mysql");
    let text = std::fs::read_to_string(fixture_root().join("basics/null_order.test")).unwrap();
    let records = parse(&text).unwrap();
    let for_mysql = records
        .iter()
        .filter(|r| !r.conditions.is_empty() && r.applies_to("mysql"))
        .count();
    let for_pg = records
        .iter()
        .filter(|r| !r.conditions.is_empty() && r.applies_to("postgresql"))
        .count();
    assert_eq!((for_mysql, for_pg), (1, 0));
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// The public corpus. Environment:
/// `SLT_CORPUS_DIR` (required, the corpus's `test` directory), `SLT_INTERFACE`
/// (`embedded`, `pgwire`, `mysqlwire`), `SLT_FILTER` (part of a path), `SLT_MAX_FILES`,
/// `SLT_JOBS`, `SLT_TIMEOUT_MS` (per record), `SLT_OUT_DIR`, `SLT_BASELINE`
/// (`off`, `check`, `update`), `SLT_CORPUS_NAME`, `SLT_SOURCE_REVISION`.
#[test]
#[ignore = "needs the SQL Logic Test corpus: run scripts/slt_fetch_corpus.sh, then scripts/slt_run.sh"]
fn the_public_corpus() {
    let root = PathBuf::from(
        std::env::var("SLT_CORPUS_DIR").expect("SLT_CORPUS_DIR is not set: run scripts/slt_fetch_corpus.sh"),
    );
    let interface = Interface::parse(&env_or("SLT_INTERFACE", "embedded")).expect("SLT_INTERFACE");
    let filter = std::env::var("SLT_FILTER").ok();
    let max_files: Option<usize> = std::env::var("SLT_MAX_FILES").ok().map(|n| n.parse().unwrap());
    let jobs: usize = env_or("SLT_JOBS", "4").parse().unwrap();
    let limits = Limits {
        timeout: Duration::from_millis(env_or("SLT_TIMEOUT_MS", "10000").parse().unwrap()),
        file_timeout: Duration::from_secs(env_or("SLT_FILE_TIMEOUT_S", "7200").parse().unwrap()),
        max_incidents: env_or("SLT_MAX_INCIDENTS", "20").parse().unwrap(),
    };
    let out_dir = PathBuf::from(env_or(
        "SLT_OUT_DIR",
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/correctness")
            .to_string_lossy(),
    ));
    let mut files = test_files(&root);
    assert!(!files.is_empty(), "no .test file under {}", root.display());
    let all = files.len();
    if let Some(f) = &filter {
        files.retain(|(_, name)| name.contains(f.as_str()));
    }
    if let Some(n) = max_files {
        files.truncate(n);
    }
    let partial = files.len() != all;
    let scratch = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (outcomes, incidents) = run_corpus(&files, interface, &limits, jobs, scratch.path());
    let mut corpus = env_or("SLT_CORPUS_NAME", "SQL Logic Test corpus");
    if partial {
        corpus.push_str(&format!(" (partial run: {} of {all} files)", files.len()));
    }
    let result = RunResult::build(
        interface.name(),
        interface.engine_label(),
        &corpus,
        &env_or("SLT_SOURCE_REVISION", "unknown"),
        &outcomes,
        incidents,
    );
    eprintln!(
        "slt {}: {:?} in {:.0} s, {} failure classes, {} incidents",
        interface.name(),
        result.totals,
        started.elapsed().as_secs_f64(),
        result.failure_classes.len(),
        result.incidents.len()
    );
    std::fs::create_dir_all(&out_dir).unwrap();
    let json_path = out_dir.join(format!("sqllogictest.{}.json", interface.name()));
    std::fs::write(&json_path, serde_json::to_string_pretty(&result).unwrap() + "\n").unwrap();

    // The page is rebuilt from every result document in the directory.
    let mut results = Vec::new();
    for name in ["embedded", "pgwire", "mysqlwire"] {
        if let Ok(text) = std::fs::read_to_string(out_dir.join(format!("sqllogictest.{name}.json"))) {
            results.push(serde_json::from_str::<RunResult>(&text).unwrap());
        }
    }
    std::fs::write(out_dir.join("sqllogictest.md"), render_page(&results)).unwrap();

    let baseline_path = out_dir.join("sqllogictest_baseline.json");
    let mut baseline: Baseline = std::fs::read_to_string(&baseline_path)
        .ok()
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or_default();
    match env_or("SLT_BASELINE", "off").as_str() {
        "off" => {}
        "update" => {
            raise_baseline(&mut baseline, &result);
            std::fs::write(&baseline_path, serde_json::to_string_pretty(&baseline).unwrap() + "\n").unwrap();
        }
        "check" => {
            let mut problems = check_baseline(&baseline, &result);
            if partial {
                problems.retain(|p| !p.contains("missing from the run"));
            }
            assert!(
                problems.is_empty(),
                "pass counts fell below the baseline: {problems:#?}"
            );
        }
        other => panic!("SLT_BASELINE={other}: expected off, check or update"),
    }
}
