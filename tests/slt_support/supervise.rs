//! One process per script. The parent starts this test binary again in its worker role
//! for each file; a record that kills the worker or runs past the time limit is recorded
//! and the file is run again without it, so one bad record costs one record.

use super::engines::Interface;
use super::run::{run_script, FileOutcome, Incident};
use super::script::{parse, Kind, Record};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The environment variable that turns a start of this binary into a worker; its value
/// is the path of the worker's instructions.
pub const WORKER_ENV: &str = "HELIOS_SLT_WORKER_SPEC";
/// The test function that hosts the worker role.
pub const WORKER_TEST: &str = "slt_worker";
const EXIT_TIMED_OUT: i32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub file: PathBuf,
    pub name: String,
    pub interface: String,
    /// line -> `timed_out` | `crashed`, from earlier attempts on this file
    pub skip: HashMap<usize, String>,
    pub timeout_ms: u64,
    pub progress: PathBuf,
    pub out: PathBuf,
    /// Self-test only: make the worker die (`abort`) or stall (`hang`) at this line.
    pub fault: Option<(usize, String)>,
}

/// The worker role. Never returns.
pub fn worker_main(spec_path: &str) -> ! {
    let code = match worker(spec_path) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "slt worker: {e}");
            4
        }
    };
    std::process::exit(code)
}

fn worker(spec_path: &str) -> Result<(), String> {
    let spec: WorkerSpec = serde_json::from_str(&std::fs::read_to_string(spec_path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&std::fs::read(&spec.file).map_err(|e| e.to_string())?).into_owned();
    let records = parse(&text)?;
    let interface = Interface::parse(&spec.interface).ok_or("unknown interface")?;

    // The line being run and when it started, for the watchdog; the line also goes to a
    // file so the parent knows where a dead worker was.
    let line = Arc::new(AtomicU64::new(0));
    let started = Arc::new(AtomicU64::new(0));
    let base = Instant::now();
    {
        let (line, started) = (Arc::clone(&line), Arc::clone(&started));
        let limit = spec.timeout_ms;
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(20));
            if line.load(Ordering::SeqCst) != 0 {
                let ran = (base.elapsed().as_millis() as u64).saturating_sub(started.load(Ordering::SeqCst));
                if ran > limit {
                    std::process::exit(EXIT_TIMED_OUT);
                }
            }
        });
    }
    let mut progress = std::fs::File::create(&spec.progress).map_err(|e| e.to_string())?;
    let mut engine = interface.open()?;
    let fault = spec.fault.clone();
    let mut on_record = |at: usize| {
        started.store(base.elapsed().as_millis() as u64, Ordering::SeqCst);
        line.store(at as u64, Ordering::SeqCst);
        use std::io::Seek;
        let _ = progress.seek(std::io::SeekFrom::Start(0));
        let _ = progress.write_all(&(at as u64).to_le_bytes());
        if let Some((fault_line, kind)) = &fault {
            if *fault_line == at {
                if kind == "abort" {
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    };
    let outcome = run_script(
        engine.as_mut(),
        interface.engine_label(),
        &spec.name,
        &records,
        &spec.skip,
        &mut on_record,
    );
    line.store(0, Ordering::SeqCst);
    let body = serde_json::to_string(&outcome).map_err(|e| e.to_string())?;
    std::fs::write(&spec.out, body).map_err(|e| e.to_string())?;
    Ok(())
}

/// Limits of one supervised file.
#[derive(Debug, Clone)]
pub struct Limits {
    /// per record
    pub timeout: Duration,
    /// per attempt on a file, as a last resort
    pub file_timeout: Duration,
    /// how many records of one file may time out or crash before the file is given up
    pub max_incidents: usize,
}

fn countable(records: &[Record], engine_label: &str) -> u64 {
    records
        .iter()
        .filter(|r| matches!(r.kind, Kind::Statement { .. } | Kind::Query { .. }) && r.applies_to(engine_label))
        .count() as u64
}

/// Run one file in worker processes until it completes or is given up.
pub fn run_file(
    file: &Path,
    name: &str,
    interface: Interface,
    limits: &Limits,
    scratch: &Path,
    fault: Option<(usize, String)>,
) -> (FileOutcome, Vec<Incident>) {
    let mut incidents = Vec::new();
    let text = match std::fs::read(file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return (unreadable(name, &e.to_string()), incidents),
    };
    let records = match parse(&text) {
        Ok(r) => r,
        Err(e) => return (unreadable(name, &e), incidents),
    };
    let by_line: HashMap<usize, &Record> = records.iter().map(|r| (r.line, r)).collect();
    let tag = name.replace(['/', '.'], "_");
    let spec_path = scratch.join(format!("{tag}.spec.json"));
    let mut spec = WorkerSpec {
        file: file.to_path_buf(),
        name: name.to_string(),
        interface: interface.name().to_string(),
        skip: HashMap::new(),
        timeout_ms: limits.timeout.as_millis() as u64,
        progress: scratch.join(format!("{tag}.progress")),
        out: scratch.join(format!("{tag}.out.json")),
        fault,
    };
    let stderr_path = scratch.join(format!("{tag}.stderr"));
    let mut last_kind = "crashed".to_string();
    loop {
        let _ = std::fs::remove_file(&spec.out);
        let _ = std::fs::remove_file(&spec.progress);
        let attempt = attempt(&spec, &spec_path, &stderr_path, limits.file_timeout);
        let detail = match attempt {
            Ok(outcome) => {
                cleanup(&[&spec_path, &spec.progress, &spec.out, &stderr_path]);
                return (outcome, incidents);
            }
            Err((kind, detail)) => {
                last_kind = kind;
                detail
            }
        };
        let at = std::fs::read(&spec.progress)
            .ok()
            .and_then(|b| b.get(..8).map(|s| s.to_vec()))
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_le_bytes(a) as usize
            })
            .unwrap_or(0);
        let sql = by_line.get(&at).map(|r| r.sql.clone()).unwrap_or_default();
        incidents.push(Incident {
            file: name.to_string(),
            line: at,
            kind: last_kind.clone(),
            sql,
            detail,
        });
        if at == 0 || spec.skip.contains_key(&at) || spec.skip.len() >= limits.max_incidents {
            break;
        }
        spec.skip.insert(at, last_kind.clone());
    }
    // Given up: every record of the file that applies goes to the count of the last incident.
    cleanup(&[&spec_path, &spec.progress, &spec.out, &stderr_path]);
    let mut outcome = FileOutcome::default();
    let n = countable(&records, interface.engine_label());
    if last_kind == "crashed" {
        outcome.counts.crashed = n;
    } else {
        outcome.counts.timed_out = n;
    }
    outcome.counts.skipped = records
        .iter()
        .filter(|r| matches!(r.kind, Kind::Statement { .. } | Kind::Query { .. }))
        .count() as u64
        - n;
    incidents.push(Incident {
        file: name.to_string(),
        line: 0,
        kind: last_kind,
        sql: String::new(),
        detail: "file given up: too many records timed out or crashed, or the worker died outside a record".to_string(),
    });
    (outcome, incidents)
}

fn cleanup(paths: &[&PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

fn unreadable(name: &str, why: &str) -> FileOutcome {
    let mut outcome = FileOutcome::default();
    outcome.counts.failed = 1;
    outcome.classes.insert(
        "script the runner cannot read".to_string(),
        super::run::FailureClass {
            count: 1,
            example: super::run::Example {
                file: name.to_string(),
                line: 0,
                sql: String::new(),
                detail: why.to_string(),
            },
        },
    );
    outcome
}

fn attempt(
    spec: &WorkerSpec,
    spec_path: &Path,
    stderr_path: &Path,
    file_timeout: Duration,
) -> Result<FileOutcome, (String, String)> {
    let crashed = |d: String| ("crashed".to_string(), d);
    std::fs::write(
        spec_path,
        serde_json::to_string(spec).map_err(|e| crashed(e.to_string()))?,
    )
    .map_err(|e| crashed(e.to_string()))?;
    let stderr = std::fs::File::create(stderr_path).map_err(|e| crashed(e.to_string()))?;
    let exe = std::env::current_exe().map_err(|e| crashed(e.to_string()))?;
    let mut child = std::process::Command::new(exe)
        .args(["--exact", WORKER_TEST, "--nocapture", "--test-threads", "1"])
        .env(WORKER_ENV, spec_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| crashed(e.to_string()))?;
    let deadline = Instant::now() + file_timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(("timed_out".to_string(), "the file's time limit ran out".to_string()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(crashed(e.to_string())),
        }
    };
    if status.success() {
        let body = std::fs::read_to_string(&spec.out).map_err(|e| crashed(format!("no result: {e}")))?;
        return serde_json::from_str(&body).map_err(|e| crashed(format!("unreadable result: {e}")));
    }
    if status.code() == Some(EXIT_TIMED_OUT) {
        return Err((
            "timed_out".to_string(),
            format!("no answer within {} ms", spec.timeout_ms),
        ));
    }
    let mut tail = String::new();
    if let Ok(mut f) = std::fs::File::open(stderr_path) {
        let mut all = String::new();
        let _ = f.read_to_string(&mut all);
        let lines: Vec<&str> = all.lines().filter(|l| !l.trim().is_empty()).collect();
        tail = lines
            .iter()
            .rev()
            .take(4)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join(" / ");
    }
    Err(crashed(format!("worker ended with {status}: {tail}")))
}
