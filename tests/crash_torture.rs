#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Crash-recovery torture, stage 1: a process crash.
//!
//! A seeded workload runs in a worker process against a database on disk; the worker is
//! killed with SIGKILL; a fresh process opens the database and reports what it holds; the
//! parent compares that with a model of what was acknowledged. See
//! `tests/crash_torture_support/harness.rs`.
//!
//! The short run here uses a fixed list of seeds. `scripts/crash_torture_run.sh` runs
//! many more and writes `docs/correctness/crash-recovery.md`.

#[path = "crash_torture_support/harness.rs"]
mod harness;
#[path = "crash_torture_support/workload.rs"]
mod workload;

use harness::{run_iteration, IterationReport, Kill, KillPoint, Plan, RunReport, Sabotage, CHILD_ENV};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use workload::{op, statements, Model, Op, ACCOUNTS, OPENING_BALANCE};

/// The worker, setup and observer roles. Without the environment variable this is an
/// empty test.
#[test]
fn crash_torture_child() {
    if let Ok(spec) = std::env::var(CHILD_ENV) {
        harness::child_main(&spec);
    }
}

#[test]
fn the_workload_is_a_function_of_the_seed_and_keeps_the_sum() {
    let mut kinds = std::collections::BTreeSet::new();
    let mut model = Model::opening();
    for n in 1..=2_000 {
        let a = op(7, n);
        assert_eq!(a, op(7, n), "the same seed and number give the same operation");
        kinds.insert(a.kind());
        let stmts = statements(n, &a);
        match &a {
            Op::Notes { .. } | Op::Owner { .. } => assert_eq!(stmts.len(), 1),
            Op::Abandoned { .. } => assert_eq!(stmts.last().unwrap(), "ROLLBACK"),
            _ => assert_eq!((stmts[0].as_str(), stmts.last().unwrap().as_str()), ("BEGIN", "COMMIT")),
        }
        model.apply(n, &a);
        assert_eq!(model.total(), ACCOUNTS * OPENING_BALANCE, "after operation {n}");
    }
    assert_eq!(kinds.len(), 5, "every kind of operation appears: {kinds:?}");
    assert_ne!(op(7, 1), op(8, 1), "another seed is another workload");
    assert!(!model.transfers.is_empty() && !model.notes.is_empty() && !model.ledger.is_empty());
}

fn run_seeds(seeds: &[u64], plan: &Plan) -> Vec<IterationReport> {
    let scratch = tempfile::tempdir().unwrap();
    let next = AtomicU64::new(0);
    let out = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst) as usize;
                let Some(seed) = seeds.get(at) else { break };
                let report = run_iteration(*seed, plan, scratch.path());
                out.lock().unwrap().push(report);
            });
        }
    });
    let mut reports = out.into_inner().unwrap();
    reports.sort_by_key(|r| r.seed);
    reports
}

fn violations(reports: &[IterationReport]) -> Vec<String> {
    reports.iter().flat_map(|r| r.violations.clone()).collect()
}

/// The gate: a fixed list of seeds, every kind of kill, no violation.
#[test]
fn a_killed_process_loses_no_acknowledged_commit_and_leaves_no_partial_statement() {
    let seeds: Vec<u64> = (5_000..5_024).collect();
    let plan = Plan::default();
    let reports = run_seeds(&seeds, &plan);
    assert_eq!(reports.len(), seeds.len());
    let found = violations(&reports);
    assert!(found.is_empty(), "crash-consistency violations:\n{}", found.join("\n"));

    // The run must have exercised what it claims to.
    let rounds: Vec<&harness::RoundReport> = reports.iter().flat_map(|r| r.rounds.iter()).collect();
    assert_eq!(rounds.len() as u64, seeds.len() as u64 * plan.rounds);
    for point in KillPoint::ALL {
        assert!(
            rounds
                .iter()
                .any(|r| matches!(&r.kill, Kill::Named(p, _) if *p == point)),
            "no round was killed at {}",
            point.name()
        );
    }
    assert!(
        rounds.iter().any(|r| matches!(r.kill, Kill::Timed(_))),
        "no round was killed from outside"
    );
    assert!(
        rounds.iter().any(|r| r.in_flight_applied == Some(true)),
        "no in-flight operation survived"
    );
    assert!(
        rounds.iter().any(|r| r.in_flight_applied == Some(false)),
        "no in-flight operation was lost"
    );
    let acknowledged: usize = rounds.iter().map(|r| r.acknowledged.len()).sum();
    assert!(acknowledged > 200, "only {acknowledged} operations were acknowledged");
}

/// The same with a time-travel read of the opening state after every restart.
#[test]
fn time_travel_to_the_opening_state_answers_after_every_restart() {
    let plan = Plan {
        time_travel: true,
        ..Plan::default()
    };
    let found = violations(&run_seeds(&[6_001, 6_002], &plan));
    assert!(found.is_empty(), "violations:\n{}", found.join("\n"));
}

/// A kill at a named point is replayable: the same seed dies at the same operation and
/// leaves the same acknowledged set and the same answer about the operation in flight.
#[test]
fn a_seed_replays_to_the_same_kill_point() {
    let plan = Plan {
        timed_percent: 0,
        ..Plan::default()
    };
    let first = run_seeds(&[7_001, 7_002, 7_003], &plan);
    let second = run_seeds(&[7_001, 7_002, 7_003], &plan);
    assert_eq!(first, second);
    assert!(violations(&first).is_empty(), "{:?}", violations(&first));
    assert!(first.iter().all(|r| r.rounds.len() as u64 == plan.rounds));
}

fn sabotaged(how: Sabotage) -> Vec<String> {
    let plan = Plan {
        timed_percent: 0,
        sabotage: Some(how),
        ..Plan::default()
    };
    violations(&run_seeds(&[8_001], &plan))
}

/// The checker can fail: a transfer applied by halves is caught.
#[test]
fn the_checker_catches_a_half_applied_transfer() {
    let found = sabotaged(Sabotage::SplitTransfer);
    assert!(
        found.iter().any(|v| v.contains("neither the acknowledged state nor")),
        "{found:?}"
    );
    assert!(found.iter().any(|v| v.contains("accounts")), "{found:?}");
}

/// The checker can fail: an acknowledged operation that is not there is caught.
#[test]
fn the_checker_catches_an_acknowledged_commit_that_is_missing() {
    let found = sabotaged(Sabotage::AckWithoutCommit);
    assert!(
        found.iter().any(|v| v.contains("neither the acknowledged state nor")),
        "{found:?}"
    );
    assert!(found.iter().any(|v| v.contains("expected and not found")), "{found:?}");
}

/// The checker can fail: half of a multi-row statement is caught.
#[test]
fn the_checker_catches_a_partly_applied_statement() {
    let found = sabotaged(Sabotage::HalfStatement);
    assert!(found.iter().any(|v| v.contains("notes")), "{found:?}");
}

/// The long run, started by `scripts/crash_torture_run.sh`: `CRASH_TORTURE_ITERATIONS`
/// seeds from `CRASH_TORTURE_SEED`, at most `CRASH_TORTURE_BUDGET_S` seconds,
/// `CRASH_TORTURE_JOBS` at a time; writes `crash-recovery.json` and `crash-recovery.md`
/// into `CRASH_TORTURE_OUT_DIR`. Without `CRASH_TORTURE_ITERATIONS` this is an empty test.
#[test]
fn the_long_run_when_asked() {
    let Ok(iterations) = std::env::var("CRASH_TORTURE_ITERATIONS") else {
        return;
    };
    let iterations: u64 = iterations.parse().expect("CRASH_TORTURE_ITERATIONS");
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let first_seed: u64 = env("CRASH_TORTURE_SEED", "1").parse().unwrap();
    let budget = Duration::from_secs(env("CRASH_TORTURE_BUDGET_S", "3600").parse().unwrap());
    let jobs: usize = env("CRASH_TORTURE_JOBS", "4").parse().unwrap();
    let out_dir = std::path::PathBuf::from(env(
        "CRASH_TORTURE_OUT_DIR",
        concat!(env!("CARGO_MANIFEST_DIR"), "/docs/correctness"),
    ));
    let scratch = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let next = AtomicU64::new(0);
    let report = Mutex::new(RunReport {
        source_revision: env("CRASH_TORTURE_REVISION", "unknown"),
        toolchain: env("CRASH_TORTURE_TOOLCHAIN", "unknown"),
        first_seed,
        iterations_asked: iterations,
        ..RunReport::default()
    });
    let done = Mutex::new(std::collections::BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                if at >= iterations || started.elapsed() > budget {
                    break;
                }
                let seed = first_seed + at;
                // Every eighth seed also checks the time-travel read.
                let plan = Plan {
                    time_travel: seed % 8 == 0,
                    ..Plan::default()
                };
                let it = run_iteration(seed, &plan, scratch.path());
                for v in &it.violations {
                    eprintln!("crash torture: {v}");
                }
                let mut done = done.lock().unwrap();
                done.insert(at, it);
                if done.len() % 50 == 0 {
                    eprintln!("crash torture: {} of {iterations} seeds", done.len());
                }
            });
        }
    });
    // Only the unbroken run of seeds from the first counts, so the page can name a range.
    let done = done.into_inner().unwrap();
    let mut report = report.into_inner().unwrap();
    for (expected, (at, it)) in done.iter().enumerate() {
        if *at != expected as u64 {
            break;
        }
        report.add(it);
    }
    report.seconds = started.elapsed().as_secs();
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(
        out_dir.join("crash-recovery.json"),
        serde_json::to_string_pretty(&report).unwrap() + "\n",
    )
    .unwrap();
    std::fs::write(out_dir.join("crash-recovery.md"), report.page()).unwrap();
    eprintln!(
        "crash torture: {} seeds, {} rounds, {} failing seeds, {} s",
        report.iterations_run,
        report.rounds,
        report.failures.len(),
        report.seconds
    );
    assert!(
        report.failures.is_empty(),
        "{} seeds failed; see {}",
        report.failures.len(),
        out_dir.display()
    );
}
