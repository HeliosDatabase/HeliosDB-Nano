//! The crash harness: a worker process runs the seeded workload against a database on
//! disk and writes down what was acknowledged; it is killed (by itself at a named point,
//! or from outside at a seeded moment); an observer process opens the database again and
//! writes down what it finds; the parent compares that with the model.
//!
//! Nothing in the engine is touched: the kill is a SIGKILL of a whole process, and the
//! named points are places in the worker's own loop.

use super::workload::{op, setup_statements, statements, Model, Op, Rng, ACCOUNTS, OPENING_BALANCE};
use heliosdb_nano::{EmbeddedDatabase, Value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The environment variable that turns a start of the test binary into a child; its
/// value is the path of the child's instructions.
pub const CHILD_ENV: &str = "HELIOS_CRASH_TORTURE_SPEC";
/// The test function that hosts the child roles.
pub const CHILD_TEST: &str = "crash_torture_child";

/// Where in its own loop the worker dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillPoint {
    /// After writing down that the operation starts, before its first statement.
    BeforeStatement,
    /// Inside the operation's transaction, before its COMMIT or ROLLBACK.
    MidTransaction,
    /// After the operation returned, before that is written down.
    AfterCommitBeforeAck,
    /// After the acknowledgement is written down.
    AfterAck,
}

impl KillPoint {
    pub const ALL: [KillPoint; 4] = [
        KillPoint::BeforeStatement,
        KillPoint::MidTransaction,
        KillPoint::AfterCommitBeforeAck,
        KillPoint::AfterAck,
    ];

    pub fn name(self) -> &'static str {
        match self {
            KillPoint::BeforeStatement => "before_statement",
            KillPoint::MidTransaction => "mid_transaction",
            KillPoint::AfterCommitBeforeAck => "after_commit_before_ack",
            KillPoint::AfterAck => "after_ack",
        }
    }
}

/// A deliberate fault in the worker, to prove that the checker can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sabotage {
    /// A transfer run as two separate statements, killed between them.
    SplitTransfer,
    /// An operation acknowledged without being run.
    AckWithoutCommit,
    /// A multi-row insert run as two statements, killed between them.
    HalfStatement,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Role {
    Setup,
    Work {
        seed: u64,
        first_op: u64,
        ops: u64,
        kill: Option<(KillPoint, u64)>,
        sabotage: Option<(Sabotage, u64)>,
    },
    Observe {
        as_of: Option<String>,
        out: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSpec {
    pub dir: PathBuf,
    pub log: PathBuf,
    pub role: Role,
}

/// End this process the way a crash does: SIGKILL, no destructor, no flush.
fn die() -> ! {
    let _ = std::process::Command::new("kill")
        .args(["-9", &std::process::id().to_string()])
        .status();
    std::thread::sleep(Duration::from_secs(5));
    std::process::abort()
}

struct Log(std::fs::File);

impl Log {
    /// One line, handed to the kernel before returning: a killed process loses nothing
    /// the kernel already has.
    fn line(&mut self, text: &str) {
        let _ = self.0.write_all(format!("{text}\n").as_bytes());
    }
}

/// The child roles. Never returns.
pub fn child_main(spec_path: &str) -> ! {
    let spec: ChildSpec = match std::fs::read_to_string(spec_path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "crash torture child: {e}");
            std::process::exit(6)
        }
    };
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&spec.log) else {
        std::process::exit(6)
    };
    let mut log = Log(file);
    let db = match EmbeddedDatabase::new(&spec.dir) {
        Ok(db) => db,
        Err(e) => {
            log.line(&format!("X open: {}", e.to_string().replace('\n', " ")));
            std::process::exit(5)
        }
    };
    match spec.role {
        Role::Setup => {
            for sql in setup_statements() {
                if let Err(e) = db.execute(&sql) {
                    log.line(&format!("X setup: {sql}: {}", e.to_string().replace('\n', " ")));
                    std::process::exit(5);
                }
            }
            log.line("D");
        }
        Role::Work {
            seed,
            first_op,
            ops,
            kill,
            sabotage,
        } => {
            log.line("R");
            for n in first_op..first_op + ops {
                let this = op(seed, n);
                let stmts = statements(n, &this);
                log.line(&format!("I {n}"));
                if let Some((how, at)) = sabotage {
                    if at == n {
                        sabotaged(&db, &mut log, how, n, &this);
                    }
                }
                let killed_here = |point: KillPoint| kill == Some((point, n));
                if killed_here(KillPoint::BeforeStatement)
                    || (killed_here(KillPoint::MidTransaction) && stmts.len() == 1)
                {
                    die();
                }
                let mut failed = None;
                for (i, sql) in stmts.iter().enumerate() {
                    if killed_here(KillPoint::MidTransaction) && i + 1 == stmts.len() {
                        die();
                    }
                    if let Err(e) = db.execute(sql) {
                        failed = Some(format!("{sql}: {}", e.to_string().replace('\n', " ")));
                        if stmts.len() > 1 && i > 0 {
                            let _ = db.execute("ROLLBACK");
                        }
                        break;
                    }
                }
                if killed_here(KillPoint::AfterCommitBeforeAck) {
                    die();
                }
                match failed {
                    Some(why) => log.line(&format!("E {n} {why}")),
                    None => log.line(&format!("A {n}")),
                }
                if killed_here(KillPoint::AfterAck) {
                    die();
                }
            }
            log.line("D");
        }
        Role::Observe { as_of, out } => {
            let seen = observe(&db, as_of.as_deref());
            let body = serde_json::to_string(&seen).unwrap_or_default();
            if std::fs::write(&out, body).is_err() {
                std::process::exit(6);
            }
        }
    }
    // No close: the process ends with the database open, as a crash would leave it.
    std::process::exit(0)
}

fn sabotaged(db: &EmbeddedDatabase, log: &mut Log, how: Sabotage, n: u64, this: &Op) -> ! {
    match (how, this) {
        (Sabotage::SplitTransfer, Op::Transfer { src, amount, .. }) => {
            let _ = db.execute(&format!(
                "UPDATE accounts SET balance = balance - {amount} WHERE id = {src}"
            ));
        }
        (Sabotage::HalfStatement, Op::Notes { rows }) => {
            let values: Vec<String> = (0..rows / 2).map(|k| format!("({n}, {k}, 'note {n}/{k}')")).collect();
            let _ = db.execute(&format!(
                "INSERT INTO notes (opno, k, body) VALUES {}",
                values.join(", ")
            ));
        }
        (Sabotage::AckWithoutCommit, _) => log.line(&format!("A {n}")),
        _ => log.line(&format!("X sabotage {how:?} does not fit operation {n}")),
    }
    die()
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Numeric(n) => n.clone(),
        other => other.to_string(),
    }
}

type Rows = Result<Vec<Vec<String>>, String>;

/// What the observer found. Facts only; the parent judges them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub accounts: Option<Rows>,
    pub transfers: Option<Rows>,
    pub notes: Option<Rows>,
    pub ledger: Option<Rows>,
    /// name of an index-backed question -> its answer
    pub lookups: BTreeMap<String, Rows>,
    pub sum: Option<Rows>,
    /// The insert of a transfer from an account that does not exist: must be an error.
    pub orphan_insert: Option<Result<u64, String>>,
    pub branch: Option<Rows>,
    pub back_on_main: Option<Rows>,
    pub as_of: Option<Rows>,
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Rows {
    db.query(sql, &[])
        .map(|rows| rows.iter().map(|t| t.values.iter().map(text).collect()).collect())
        .map_err(|e| e.to_string())
}

fn observe(db: &EmbeddedDatabase, as_of: Option<&str>) -> Observation {
    let mut seen = Observation::default();
    let accounts = rows(db, "SELECT id, balance, owner FROM accounts ORDER BY id");
    let transfers = rows(db, "SELECT id, src, dst, amount, opno FROM transfers ORDER BY opno, id");
    let notes = rows(db, "SELECT opno, k, body FROM notes ORDER BY opno, k");
    seen.ledger = Some(rows(db, "SELECT opno, kind FROM ledger ORDER BY opno"));
    seen.sum = Some(rows(db, "SELECT count(*), sum(balance) FROM accounts"));

    // Questions an index answers, to be compared with the full reads above.
    for id in 1..=ACCOUNTS {
        seen.lookups.insert(
            format!("accounts.id={id}"),
            rows(db, &format!("SELECT id, balance, owner FROM accounts WHERE id = {id}")),
        );
    }
    if let Ok(all) = &accounts {
        let balances: BTreeSet<&String> = all.iter().filter_map(|r| r.get(1)).collect();
        for b in balances {
            seen.lookups.insert(
                format!("accounts.balance={b}"),
                rows(db, &format!("SELECT id FROM accounts WHERE balance = {b} ORDER BY id")),
            );
        }
        seen.lookups.insert(
            format!("accounts.balance>={OPENING_BALANCE}"),
            rows(
                db,
                &format!("SELECT id FROM accounts WHERE balance >= {OPENING_BALANCE} ORDER BY id"),
            ),
        );
    }
    if let Ok(all) = &transfers {
        for opno in all.iter().filter_map(|r| r.get(4)).collect::<BTreeSet<_>>() {
            seen.lookups.insert(
                format!("transfers.opno={opno}"),
                rows(db, &format!("SELECT id FROM transfers WHERE opno = {opno} ORDER BY id")),
            );
        }
    }
    for k in [0, 1, 7, 20] {
        seen.lookups.insert(
            format!("notes.k={k}"),
            rows(db, &format!("SELECT opno FROM notes WHERE k = {k} ORDER BY opno")),
        );
    }
    if let Ok(all) = &notes {
        for opno in all.iter().filter_map(|r| r.first()).collect::<BTreeSet<_>>() {
            seen.lookups.insert(
                format!("notes.opno={opno}"),
                rows(db, &format!("SELECT k FROM notes WHERE opno = {opno} ORDER BY k")),
            );
        }
    }
    seen.accounts = Some(accounts);
    seen.transfers = Some(transfers);
    seen.notes = Some(notes);

    let orphan = db
        .execute("INSERT INTO transfers (src, dst, amount, opno) VALUES (9999, 1, 1, -1)")
        .map_err(|e| e.to_string());
    if orphan.is_ok() {
        let _ = db.execute("DELETE FROM transfers WHERE opno = -1");
    }
    seen.orphan_insert = Some(orphan);

    if let Some(ts) = as_of {
        seen.as_of = Some(rows(
            db,
            &format!("SELECT count(*), sum(balance) FROM accounts AS OF TIMESTAMP '{ts}'"),
        ));
    }
    seen.branch = Some(match db.execute("USE BRANCH opening") {
        Ok(_) => rows(db, "SELECT count(*), sum(balance) FROM accounts"),
        Err(e) => Err(format!("USE BRANCH opening: {e}")),
    });
    seen.back_on_main = Some(match db.execute("USE BRANCH main") {
        Ok(_) => rows(db, "SELECT count(*) FROM ledger"),
        Err(e) => Err(format!("USE BRANCH main: {e}")),
    });
    seen
}

// ---------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------

/// How the worker of a round is killed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kill {
    /// By itself, at a point of its loop: the same seed dies at the same place.
    Named(KillPoint, u64),
    /// From outside, this many microseconds after it reported ready.
    Timed(u64),
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub rounds: u64,
    pub ops_per_round: u64,
    /// Out of 100 rounds, how many are killed from outside at a seeded moment.
    pub timed_percent: u64,
    /// Also check a time-travel read of the opening state (costs about two seconds).
    pub time_travel: bool,
    pub sabotage: Option<Sabotage>,
    pub child_timeout: Duration,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            rounds: 3,
            ops_per_round: 30,
            timed_percent: 40,
            time_travel: false,
            sabotage: None,
            child_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundReport {
    pub round: u64,
    pub kill: Kill,
    pub first_op: u64,
    pub acknowledged: Vec<u64>,
    /// The operation that had started and was not acknowledged, if any.
    pub in_flight: Option<u64>,
    /// Whether the database held the in-flight operation after the restart.
    pub in_flight_applied: Option<bool>,
    pub worker_finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationReport {
    pub seed: u64,
    pub rounds: Vec<RoundReport>,
    pub violations: Vec<String>,
}

fn spawn(spec: &ChildSpec, scratch: &Path, tag: &str) -> Result<std::process::Child, String> {
    let spec_path = scratch.join(format!("{tag}.spec.json"));
    std::fs::write(&spec_path, serde_json::to_string(spec).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let stderr = std::fs::File::create(scratch.join(format!("{tag}.stderr"))).map_err(|e| e.to_string())?;
    std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads", "1"])
        .env(CHILD_ENV, &spec_path)
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| e.to_string())
}

/// Wait for a child; `None` when it had to be killed for running past the limit.
fn wait(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => return None,
        }
    }
}

fn stderr_tail(scratch: &Path, tag: &str) -> String {
    let all = std::fs::read_to_string(scratch.join(format!("{tag}.stderr"))).unwrap_or_default();
    let lines: Vec<&str> = all.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(6)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join(" / ")
}

struct LogView {
    ready: bool,
    done: bool,
    acknowledged: Vec<u64>,
    errors: Vec<String>,
    in_flight: Option<u64>,
    fatal: Vec<String>,
}

fn read_log(path: &Path) -> LogView {
    let mut view = LogView {
        ready: false,
        done: false,
        acknowledged: Vec::new(),
        errors: Vec::new(),
        in_flight: None,
        fatal: Vec::new(),
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // A line without its newline is a line the kill cut: it does not count.
    let complete = match text.rfind('\n') {
        Some(at) => text.get(..at).unwrap_or(""),
        None => "",
    };
    for line in complete.lines() {
        let mut words = line.splitn(3, ' ');
        match (words.next(), words.next().and_then(|n| n.parse::<u64>().ok())) {
            (Some("R"), _) => view.ready = true,
            (Some("D"), _) => view.done = true,
            (Some("I"), Some(n)) => view.in_flight = Some(n),
            (Some("A"), Some(n)) => {
                view.acknowledged.push(n);
                view.in_flight = None;
            }
            (Some("E"), Some(n)) => {
                view.errors
                    .push(format!("operation {n} was refused: {}", words.next().unwrap_or("")));
                view.in_flight = None;
            }
            _ => view.fatal.push(line.to_string()),
        }
    }
    view
}

fn expect_rows(model: &Model) -> (Vec<Vec<String>>, Vec<Vec<String>>, Vec<Vec<String>>, Vec<Vec<String>>) {
    let accounts = model
        .accounts
        .iter()
        .map(|(id, (balance, owner))| vec![id.to_string(), balance.to_string(), owner.clone()])
        .collect();
    let transfers = model
        .transfers
        .iter()
        .map(|(opno, (src, dst, amount))| vec![src.to_string(), dst.to_string(), amount.to_string(), opno.to_string()])
        .collect();
    let notes = model
        .notes
        .iter()
        .map(|((opno, k), body)| vec![opno.to_string(), k.to_string(), body.clone()])
        .collect();
    let ledger = model
        .ledger
        .iter()
        .map(|(opno, kind)| vec![opno.to_string(), kind.clone()])
        .collect();
    (accounts, transfers, notes, ledger)
}

fn first_difference(name: &str, want: &[Vec<String>], got: &[Vec<String>]) -> Option<String> {
    if want == got {
        return None;
    }
    let missing: Vec<&Vec<String>> = want.iter().filter(|r| !got.contains(r)).take(3).collect();
    let extra: Vec<&Vec<String>> = got.iter().filter(|r| !want.contains(r)).take(3).collect();
    Some(format!(
        "{name}: {} rows found, {} expected; expected and not found {missing:?}; found and not expected {extra:?}",
        got.len(),
        want.len()
    ))
}

/// The differences between what the observer read and a model. Empty when they agree.
fn differences(seen: &Observation, model: &Model) -> Vec<String> {
    let (accounts, transfers, notes, ledger) = expect_rows(model);
    let mut out = Vec::new();
    let table =
        |name: &str, got: &Option<Rows>, want: &[Vec<String>], drop_first: bool, out: &mut Vec<String>| match got {
            Some(Ok(rows)) => {
                let rows: Vec<Vec<String>> = if drop_first {
                    rows.iter().map(|r| r.iter().skip(1).cloned().collect()).collect()
                } else {
                    rows.clone()
                };
                out.extend(first_difference(name, want, &rows));
            }
            Some(Err(e)) => out.push(format!("{name}: cannot be read after the restart: {e}")),
            None => out.push(format!("{name}: not read")),
        };
    table("accounts", &seen.accounts, &accounts, false, &mut out);
    // The transfer id is assigned by the database: compared separately.
    table("transfers", &seen.transfers, &transfers, true, &mut out);
    table("notes", &seen.notes, &notes, false, &mut out);
    table("ledger", &seen.ledger, &ledger, false, &mut out);
    out
}

/// Checks that hold whichever model matched: sums, index answers against the full reads,
/// the foreign key, the branch and the time-travel read.
fn invariants(seen: &Observation, model: &Model, previous_ids: &BTreeSet<i64>, time_travel: bool) -> Vec<String> {
    let mut out = Vec::new();
    let opening = vec![vec![ACCOUNTS.to_string(), (ACCOUNTS * OPENING_BALANCE).to_string()]];
    match &seen.sum {
        Some(Ok(rows)) if *rows == vec![vec![ACCOUNTS.to_string(), model.total().to_string()]] => {}
        other => out.push(format!(
            "sum of balances: {other:?}, expected {} over {ACCOUNTS} accounts",
            model.total()
        )),
    }
    if model.total() != ACCOUNTS * OPENING_BALANCE {
        out.push(format!(
            "the model's own total is {}: the workload does not keep the sum",
            model.total()
        ));
    }
    let accounts = seen.accounts.clone().and_then(Result::ok).unwrap_or_default();
    let transfers = seen.transfers.clone().and_then(Result::ok).unwrap_or_default();
    let notes = seen.notes.clone().and_then(Result::ok).unwrap_or_default();
    for (name, answer) in &seen.lookups {
        let Some((what, value)) = name.split_once(['=', '>']) else {
            continue;
        };
        let pick = |table: &[Vec<String>], column: usize, result: &[usize]| -> Vec<Vec<String>> {
            table
                .iter()
                .filter(|r| r.get(column).map(String::as_str) == Some(value))
                .map(|r| result.iter().filter_map(|c| r.get(*c).cloned()).collect())
                .collect()
        };
        let want: Vec<Vec<String>> = match what {
            "accounts.id" => pick(&accounts, 0, &[0, 1, 2]),
            "accounts.balance" if name.contains(">=") => accounts
                .iter()
                .filter(|r| {
                    r.get(1)
                        .and_then(|b| b.parse::<i64>().ok())
                        .is_some_and(|b| b >= OPENING_BALANCE)
                })
                .filter_map(|r| r.first().cloned().map(|id| vec![id]))
                .collect(),
            "accounts.balance" => pick(&accounts, 1, &[0]),
            "transfers.opno" => pick(&transfers, 4, &[0]),
            "notes.k" => pick(&notes, 1, &[0]),
            "notes.opno" => pick(&notes, 0, &[1]),
            _ => continue,
        };
        match answer {
            Ok(got) if *got == want => {}
            Ok(got) => out.push(format!(
                "index and table disagree on {name}: the lookup returns {got:?}, the full read has {want:?}"
            )),
            Err(e) => out.push(format!("lookup {name} fails after the restart: {e}")),
        }
    }
    match &seen.orphan_insert {
        Some(Err(_)) => {}
        other => out.push(format!(
            "foreign key: a transfer from an account that does not exist was not refused: {other:?}"
        )),
    }
    let mut ids = BTreeSet::new();
    for row in &transfers {
        match row.first().and_then(|id| id.parse::<i64>().ok()) {
            Some(id) if ids.insert(id) => {
                let known = row
                    .get(4)
                    .and_then(|o| o.parse::<i64>().ok())
                    .is_some_and(|o| model.transfers.contains_key(&o));
                let highest_before = previous_ids.iter().next_back().copied().unwrap_or(0);
                if known && !previous_ids.contains(&id) && id <= highest_before {
                    out.push(format!(
                        "sequence went backwards: a transfer written after the restart got id {id}, \
                         ids up to {highest_before} were handed out before"
                    ));
                }
            }
            other => out.push(format!("transfer id {other:?} is missing or handed out twice")),
        }
    }
    match &seen.branch {
        Some(Ok(rows)) if *rows == opening => {}
        other => out.push(format!(
            "branch 'opening' does not answer with the opening state: {other:?}"
        )),
    }
    match &seen.back_on_main {
        Some(Ok(rows)) if *rows == vec![vec![model.ledger.len().to_string()]] => {}
        other => out.push(format!(
            "back on main after reading the branch: ledger count {other:?}, expected {}",
            model.ledger.len()
        )),
    }
    if time_travel {
        match &seen.as_of {
            Some(Ok(rows)) if *rows == opening => {}
            other => out.push(format!(
                "time travel to the opening moment does not answer with the opening state: {other:?}"
            )),
        }
    }
    out
}

fn choose_kill(seed: u64, round: u64, first_op: u64, plan: &Plan) -> Kill {
    let mut rng = Rng::new(seed.wrapping_mul(31).wrapping_add(round).wrapping_add(0xC0FFEE));
    if rng.range(0, 99) < plan.timed_percent {
        Kill::Timed(rng.range(0, 40_000))
    } else {
        let point = KillPoint::ALL[rng.range(0, 3) as usize];
        Kill::Named(point, first_op + rng.range(0, plan.ops_per_round - 1))
    }
}

/// One seed: set up, then `plan.rounds` times run, kill, restart and check.
pub fn run_iteration(seed: u64, plan: &Plan, scratch: &Path) -> IterationReport {
    let mut report = IterationReport {
        seed,
        rounds: Vec::new(),
        violations: Vec::new(),
    };
    let dir = scratch.join(format!("db-{seed}"));
    let log = scratch.join(format!("log-{seed}-setup"));
    let _ = std::fs::remove_dir_all(&dir);
    let fail = |report: &mut IterationReport, what: String| report.violations.push(format!("seed {seed}: {what}"));

    let setup = ChildSpec {
        dir: dir.clone(),
        log: log.clone(),
        role: Role::Setup,
    };
    let tag = format!("{seed}-setup");
    let finished = spawn(&setup, scratch, &tag)
        .ok()
        .and_then(|mut c| wait(&mut c, plan.child_timeout));
    if !finished.is_some_and(|s| s.success()) {
        let view = read_log(&log);
        fail(
            &mut report,
            format!("setup did not finish: {:?} {}", view.fatal, stderr_tail(scratch, &tag)),
        );
        return report;
    }
    let as_of = if plan.time_travel {
        std::thread::sleep(Duration::from_millis(1100));
        let stamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        std::thread::sleep(Duration::from_millis(1100));
        Some(stamp)
    } else {
        None
    };

    let mut model = Model::opening();
    let mut next_op = 1u64;
    for round in 0..plan.rounds {
        let log = scratch.join(format!("log-{seed}-{round}"));
        let _ = std::fs::remove_file(&log);
        let first_op = next_op;
        let mut kill = choose_kill(seed, round, first_op, plan);
        let mut sabotage = None;
        if let (Some(how), true) = (plan.sabotage, round + 1 == plan.rounds) {
            // The first operation of the last round that the fault fits.
            let fits = |o: &Op| match how {
                Sabotage::SplitTransfer => matches!(o, Op::Transfer { .. }),
                Sabotage::HalfStatement => matches!(o, Op::Notes { .. }),
                Sabotage::AckWithoutCommit => matches!(o, Op::Transfer { .. } | Op::Notes { .. }),
            };
            let at = (first_op..first_op + 10_000)
                .find(|n| fits(&op(seed, *n)))
                .unwrap_or(first_op);
            sabotage = Some((how, at));
            kill = Kill::Named(KillPoint::AfterAck, at + 1_000_000);
        }
        let ops = match sabotage {
            Some((_, at)) => at - first_op + 1,
            None => plan.ops_per_round,
        };
        let spec = ChildSpec {
            dir: dir.clone(),
            log: log.clone(),
            role: Role::Work {
                seed,
                first_op,
                ops,
                kill: match &kill {
                    Kill::Named(point, at) => Some((*point, *at)),
                    Kill::Timed(_) => None,
                },
                sabotage,
            },
        };
        let tag = format!("{seed}-{round}-work");
        let mut child = match spawn(&spec, scratch, &tag) {
            Ok(child) => child,
            Err(e) => {
                fail(&mut report, format!("round {round}: the worker did not start: {e}"));
                return report;
            }
        };
        let status = if let Kill::Timed(micros) = &kill {
            let deadline = Instant::now() + plan.child_timeout;
            while !read_log(&log).ready && Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_micros(200));
            }
            std::thread::sleep(Duration::from_micros(*micros));
            let _ = child.kill();
            child.wait().ok()
        } else {
            wait(&mut child, plan.child_timeout)
        };
        let view = read_log(&log);
        if status.is_none() {
            fail(
                &mut report,
                format!("round {round}: the worker hung (operation {:?})", view.in_flight),
            );
            return report;
        }
        for line in &view.fatal {
            fail(
                &mut report,
                format!("round {round}: worker: {line} {}", stderr_tail(scratch, &tag)),
            );
        }
        for e in &view.errors {
            fail(&mut report, format!("round {round}: {e}"));
        }
        // A worker killed before the database was open ran nothing; the round still checks.

        let mut without = model.clone();
        for n in &view.acknowledged {
            without.apply(*n, &op(seed, *n));
        }
        let with = view.in_flight.map(|n| {
            let mut m = without.clone();
            m.apply(n, &op(seed, n));
            m
        });

        // Restart and look.
        let out = scratch.join(format!("seen-{seed}-{round}.json"));
        let _ = std::fs::remove_file(&out);
        let observe = ChildSpec {
            dir: dir.clone(),
            log: scratch.join(format!("log-{seed}-{round}-observe")),
            role: Role::Observe {
                as_of: as_of.clone(),
                out: out.clone(),
            },
        };
        let tag = format!("{seed}-{round}-observe");
        let observed = spawn(&observe, scratch, &tag)
            .ok()
            .and_then(|mut c| wait(&mut c, plan.child_timeout));
        let seen: Option<Observation> = std::fs::read_to_string(&out)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let mut round_report = RoundReport {
            round,
            kill: kill.clone(),
            first_op,
            acknowledged: view.acknowledged.clone(),
            in_flight: view.in_flight,
            in_flight_applied: None,
            worker_finished: view.done,
        };
        let Some(seen) = seen else {
            let why = match observed {
                None => "the restart hung".to_string(),
                Some(status) => format!(
                    "the restart failed ({status}): {:?} {}",
                    read_log(&observe.log).fatal,
                    stderr_tail(scratch, &tag)
                ),
            };
            report.rounds.push(round_report);
            fail(&mut report, format!("round {round} ({kill:?}): {why}"));
            return report;
        };

        let against_without = differences(&seen, &without);
        let matched = if against_without.is_empty() {
            round_report.in_flight_applied = view.in_flight.map(|_| false);
            Some(without.clone())
        } else {
            match &with {
                Some(with) if differences(&seen, with).is_empty() => {
                    round_report.in_flight_applied = Some(true);
                    Some(with.clone())
                }
                _ => None,
            }
        };
        // At a named point the answer is known exactly, not only "one of the two".
        if let (Kill::Named(point, at), Some(applied)) = (&kill, round_report.in_flight_applied) {
            if view.in_flight == Some(*at) && sabotage.is_none() {
                let committed = matches!(
                    op(seed, *at),
                    Op::Transfer { .. } | Op::Notes { .. } | Op::Owner { .. } | Op::DropNotes { .. }
                );
                let must = *point == KillPoint::AfterCommitBeforeAck && committed;
                if applied != must {
                    fail(
                        &mut report,
                        format!(
                            "round {round}: killed at {} of operation {at}: the operation is {} after the restart",
                            point.name(),
                            if applied { "present" } else { "absent" }
                        ),
                    );
                }
            }
        }
        let Some(matched) = matched else {
            let mut what = against_without;
            what.truncate(4);
            report.rounds.push(round_report);
            fail(
                &mut report,
                format!(
                    "round {round} ({kill:?}, acknowledged up to {:?}, in flight {:?}): the database is neither \
                     the acknowledged state nor that state plus the operation in flight: {what:?}",
                    view.acknowledged.last(),
                    view.in_flight
                ),
            );
            return report;
        };
        for v in invariants(&seen, &matched, &model.transfer_ids, plan.time_travel) {
            fail(&mut report, format!("round {round} ({kill:?}): {v}"));
        }
        model = matched;
        if let Some(Ok(rows)) = &seen.transfers {
            model.transfer_ids.extend(
                rows.iter()
                    .filter_map(|r| r.first().and_then(|id| id.parse::<i64>().ok())),
            );
        }
        next_op = view
            .in_flight
            .or(view.acknowledged.last().copied())
            .map_or(first_op, |n| n + 1)
            .max(first_op);
        report.rounds.push(round_report);
        if !report.violations.is_empty() {
            return report;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    report
}

/// The report of a long run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunReport {
    pub source_revision: String,
    pub toolchain: String,
    pub first_seed: u64,
    pub iterations_asked: u64,
    pub iterations_run: u64,
    pub rounds: u64,
    pub kills_by_kind: BTreeMap<String, u64>,
    pub in_flight_applied: u64,
    pub in_flight_absent: u64,
    pub failures: Vec<IterationReport>,
    pub seconds: u64,
}

impl RunReport {
    pub fn add(&mut self, it: &IterationReport) {
        self.iterations_run += 1;
        for r in &it.rounds {
            self.rounds += 1;
            let kind = match &r.kill {
                Kill::Named(point, _) => point.name().to_string(),
                Kill::Timed(_) => "timed".to_string(),
            };
            *self.kills_by_kind.entry(kind).or_insert(0) += 1;
            match r.in_flight_applied {
                Some(true) => self.in_flight_applied += 1,
                Some(false) => self.in_flight_absent += 1,
                None => {}
            }
        }
        if !it.violations.is_empty() {
            self.failures.push(it.clone());
        }
    }

    pub fn page(&self) -> String {
        let mut p = String::from("# Crash recovery: process-kill torture run\n\n");
        p.push_str("Generated by `scripts/crash_torture_run.sh`. Do not edit by hand.\n\n");
        p.push_str(
            "A worker process runs a seeded workload (transfers between accounts with a constant \
             sum, multi-row inserts and updates, deletes, rolled-back transactions) and writes down \
             each operation it was told had succeeded. It is killed with SIGKILL, either by itself \
             at a chosen point of an operation or from outside at a seeded moment. The database is \
             then opened by a fresh process and compared with a model: every acknowledged \
             operation present, the one operation in flight either wholly present or wholly \
             absent, the sum of balances unchanged, index lookups equal to full reads, the \
             foreign key still enforced, generated ids never reused, the branch taken at the \
             start still answering with the opening state.\n\n\
             **Limit:** this is a process crash. The operating system stays up, so data handed to \
             the kernel is not lost. Power loss is not covered by this run.\n\n",
        );
        p.push_str(&format!("- Source revision: `{}`\n", self.source_revision));
        p.push_str(&format!("- Toolchain: `{}`\n", self.toolchain));
        p.push_str(&format!(
            "- Seeds: {} to {} ({} of {} asked for ran within the time budget)\n",
            self.first_seed,
            self.first_seed + self.iterations_run.saturating_sub(1),
            self.iterations_run,
            self.iterations_asked
        ));
        p.push_str(&format!("- Kill and restart rounds: {}\n", self.rounds));
        p.push_str(&format!("- Wall time: {} s\n", self.seconds));
        p.push_str(&format!(
            "- Operation in flight at the kill: {} times present after the restart, {} times absent\n\n",
            self.in_flight_applied, self.in_flight_absent
        ));
        p.push_str("| Kill | Rounds |\n|---|---:|\n");
        for (kind, n) in &self.kills_by_kind {
            p.push_str(&format!("| {kind} | {n} |\n"));
        }
        p.push_str(&format!("\n## Failures: {}\n\n", self.failures.len()));
        if self.failures.is_empty() {
            p.push_str("None.\n");
        }
        for f in &self.failures {
            p.push_str(&format!("### Seed {}\n\n", f.seed));
            for v in &f.violations {
                p.push_str(&format!("- {}\n", v.replace('\n', " ")));
            }
            p.push_str(&format!(
                "\nReplay: `CRASH_TORTURE_SEED={} scripts/crash_torture_run.sh 1`\n\n",
                f.seed
            ));
        }
        p
    }
}
