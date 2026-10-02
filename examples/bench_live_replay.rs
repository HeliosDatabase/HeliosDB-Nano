//! EXTERNAL DRAFT. Copy unchanged to examples/ only after the owning gate ends.
//! Run release-only, one process at a time under the shared fleet lock/scope.
//! Times real WAL decode + live row application, excluding primary SQL setup,
//! validation, network, queue bookkeeping, and commit acknowledgment durability.
use heliosdb_nano::{
    config::WalSyncModeConfig,
    storage::{WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase, Tuple, Value,
};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type Outcome<T> = Result<T, Box<dyn std::error::Error>>;
const TABLE: &str = "replay_perf";
const WARM: &str = "SELECT id, grp, payload FROM replay_perf ORDER BY id";

struct Report {
    out: BufWriter<File>,
}
impl Report {
    fn record(&mut self, value: Json) -> Outcome<()> {
        serde_json::to_writer(&mut self.out, &value)?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(())
    }
}

fn load() -> Outcome<f64> {
    Ok(fs::read_to_string("/proc/loadavg")?
        .split_whitespace()
        .next()
        .ok_or("missing Linux load average")?
        .parse()?)
}
fn epoch_ms() -> Outcome<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}
fn numbers(tuple: &Tuple) -> Outcome<Vec<i64>> {
    tuple
        .values
        .iter()
        .map(|v| match v {
            Value::Int2(n) => Ok(i64::from(*n)),
            Value::Int4(n) => Ok(i64::from(*n)),
            Value::Int8(n) => Ok(*n),
            other => Err(format!("unexpected non-integer result {other:?}").into()),
        })
        .collect()
}
fn query(db: &EmbeddedDatabase, sql: &str) -> Outcome<Vec<Vec<i64>>> {
    db.query(sql, &[])?.iter().map(numbers).collect()
}
fn expected(n: usize, phase: &str) -> Vec<Vec<i64>> {
    if phase == "empty" || phase == "delete" {
        return Vec::new();
    }
    (1..=n as i64)
        .map(|id| {
            if phase == "update" {
                vec![id, (id + 1) % 64, id * 10 + 1]
            } else {
                vec![id, id % 64, id * 10]
            }
        })
        .collect()
}
fn database(path: &Path, storage: &str) -> Outcome<EmbeddedDatabase> {
    let mut config = if storage == "memory" {
        Config::in_memory()
    } else {
        Config::default()
    };
    if storage == "disk" {
        config.storage.path = Some(path.to_path_buf());
    }
    config.storage.wal_enabled = true;
    // Forces real per-statement WAL capture without process-global HA role state.
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_sync_mode = WalSyncModeConfig::Async;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE replay_perf (id INT PRIMARY KEY, grp INT, payload INT UNIQUE)")?;
    db.execute("CREATE INDEX replay_perf_grp ON replay_perf(grp)")?;
    Ok(db)
}

// All query observations, including failures and full actual/expected row sets,
// are written outside the timing interval. Failure never becomes a valid sample.
fn validate(db: &EmbeddedDatabase, want: &[Vec<i64>], label: &str, report: &mut Report) -> Outcome<bool> {
    let raw = db.storage.scan_table(TABLE)?;
    let mut raw_values: Vec<Vec<i64>> = raw.iter().map(numbers).collect::<Outcome<_>>()?;
    raw_values.sort();
    let mut sorted_want = want.to_vec();
    sorted_want.sort();
    let mut valid = raw_values == sorted_want;
    report.record(json!({"kind":"validation", "stage":label, "probe":"raw_storage",
        "valid":valid, "expected":sorted_want, "actual":raw_values,
        "row_ids":raw.iter().map(|row| row.row_id).collect::<Vec<_>>()}))?;
    let mut probes = vec![
        (WARM.to_string(), want.to_vec()),
        (
            "SELECT payload, grp, id FROM replay_perf WHERE id > 0 ORDER BY id".into(),
            want.iter().map(|row| vec![row[2], row[1], row[0]]).collect(),
        ),
        ("SELECT COUNT(*) FROM replay_perf".into(), vec![vec![want.len() as i64]]),
    ];
    for id in [1, want.len() as i64 / 2 + 1, want.len().max(1) as i64] {
        probes.push((
            format!("SELECT id, grp, payload FROM replay_perf WHERE id = {id}"),
            want.iter().filter(|row| row[0] == id).cloned().collect(),
        ));
    }
    for group in [0, 7, 63] {
        probes.push((
            format!("SELECT id, grp, payload FROM replay_perf WHERE grp = {group} ORDER BY id"),
            want.iter().filter(|row| row[1] == group).cloned().collect(),
        ));
    }
    // Check both old and new keys in the unique payload index.
    for payload in [10, 11] {
        probes.push((
            format!("SELECT id, grp, payload FROM replay_perf WHERE payload = {payload}"),
            want.iter().filter(|row| row[2] == payload).cloned().collect(),
        ));
    }
    for (sql, expected) in probes {
        match query(db, &sql) {
            Ok(actual) => {
                let pass = actual == expected;
                valid &= pass;
                report.record(json!({"kind":"validation", "stage":label, "sql":sql,
                    "valid":pass, "expected":expected, "actual":actual}))?;
            }
            Err(error) => {
                valid = false;
                report.record(json!({"kind":"validation", "stage":label, "sql":sql,
                    "valid":false, "expected":expected, "error":error.to_string()}))?;
            }
        }
    }
    Ok(valid)
}

fn prepare(primary: &EmbeddedDatabase, n: usize, phase: &str, report: &mut Report) -> Outcome<Vec<Vec<u8>>> {
    let before = primary.storage.wal_lsn().ok_or("logical WAL unavailable")?;
    let timer = Instant::now();
    for id in 1..=n {
        let sql = match phase {
            "insert" => format!("INSERT INTO replay_perf VALUES ({id}, {}, {})", id % 64, id * 10),
            "update" => format!(
                "UPDATE replay_perf SET grp = {}, payload = {} WHERE id = {id}",
                (id + 1) % 64,
                id * 10 + 1
            ),
            "delete" => format!("DELETE FROM replay_perf WHERE id = {id}"),
            _ => return Err("unknown benchmark phase".into()),
        };
        primary.execute(&sql)?;
    }
    let reader = WriteAheadLog::open(primary.storage.db(), WalSyncMode::Async)?;
    let entries = reader.replay_from(before)?;
    let mut row_operations = 0;
    let mut operations = Vec::new();
    for entry in entries {
        let variant_matches = match (&entry.operation, phase) {
            (WalOperation::Insert { table, .. }, "insert")
            | (WalOperation::Update { table, .. }, "update")
            | (WalOperation::Delete { table, .. }, "delete") => table == TABLE,
            _ => false,
        };
        if variant_matches {
            row_operations += 1;
        } else if !matches!(&entry.operation, WalOperation::UpdateCounter { table_name, .. } if table_name == TABLE) {
            return Err(format!("unexpected SQL-generated {phase} WAL operation {:?}", entry.operation).into());
        }
        operations.push(bincode::serialize(&entry.operation)?);
    }
    if row_operations != n {
        return Err(format!("{phase}: expected {n} real row records, got {row_operations}").into());
    }
    report.record(
        json!({"kind":"preparation", "phase":phase, "row_operations":row_operations,
        "wal_records":operations.len(), "serialized_bytes":operations.iter().map(Vec::len).sum::<usize>(),
        "elapsed_ns":timer.elapsed().as_nanos(), "excluded_from_apply_timing":true}),
    )?;
    let wanted = expected(n, phase);
    if !validate(primary, &wanted, &format!("primary_{phase}"), report)? {
        return Err("primary fixture SQL is not correct; refuse to time replay".into());
    }
    Ok(operations)
}

fn trial(n: usize, storage: &str, root: &Path, report: &mut Report) -> Outcome<bool> {
    let primary = database(&root.join("primary"), storage)?;
    let standby = database(&root.join("standby"), storage)?;
    let mut previous = "empty";
    let mut all_valid = true;
    let mut maximum_replayed_row_id = 0;
    for phase in ["insert", "update", "delete"] {
        let operations = prepare(&primary, n, phase, report)?;
        let before_rows = expected(n, previous);
        let mut pre_valid = validate(&standby, &before_rows, &format!("standby_before_{phase}"), report)?;
        for attempt in 1..=5 {
            let actual = query(&standby, WARM)?;
            let valid = actual == before_rows;
            pre_valid &= valid;
            report.record(json!({"kind":"cache_warm", "phase":phase, "attempt":attempt,
                "sql":WARM, "valid":valid, "expected":before_rows, "actual":actual}))?;
        }
        let load_before = load()?;
        if load_before >= 6.0 {
            report.record(json!({"kind":"load_gate_refusal", "phase":phase, "load1":load_before}))?;
            return Err("load1 must be below 6 before each timed phase; rerun this trial under the fleet gate".into());
        }
        let started_ms = epoch_ms()?;
        let mut raw_chunks: Vec<(usize, u128)> = Vec::with_capacity(operations.len().div_ceil(256));
        let timer = Instant::now();
        let mut chunk_start = Instant::now();
        for (index, bytes) in operations.iter().enumerate() {
            let operation: WalOperation = bincode::deserialize(bytes)?;
            if let Err(error) = standby.storage.apply_replicated_operation(operation) {
                report.record(json!({"kind":"application_error", "phase":phase, "record_index":index,
                    "elapsed_ns":timer.elapsed().as_nanos(), "eligible_for_comparison":false,
                    "error":error.to_string()}))?;
                return Err(error.into());
            }
            if (index + 1) % 256 == 0 || index + 1 == operations.len() {
                raw_chunks.push((index + 1, chunk_start.elapsed().as_nanos()));
                chunk_start = Instant::now();
            }
        }
        let elapsed_ns = timer.elapsed().as_nanos();
        let load_after = load()?;
        // Retain the raw measurement even if later validation itself errors.
        report.record(json!({"kind":"raw_timing", "phase":phase, "rows":n,
            "wal_records":operations.len(), "elapsed_ns":elapsed_ns,
            "rows_per_second":(n as f64)*1e9/(elapsed_ns as f64),
            "raw_chunk_fields":["end_record", "elapsed_ns"], "raw_chunks":raw_chunks,
            "started_unix_ms":started_ms, "load1_before":load_before, "load1_after":load_after,
            "eligible_for_comparison":false, "validation_pending":true,
            "scope":"bincode decode plus sequential live apply; no network or acknowledged fsync"}))?;
        let post_valid = validate(&standby, &expected(n, phase), &format!("standby_after_{phase}"), report)?;
        let valid = pre_valid && post_valid && load_after < 6.0;
        all_valid &= valid;
        report.record(json!({"kind":"timing_verdict", "phase":phase,
            "elapsed_ns":elapsed_ns, "precondition_valid":pre_valid, "postcondition_valid":post_valid,
            "eligible_for_comparison":valid, "eligibility_also_requires_final_trial_pass":true}))?;
        if phase == "insert" {
            maximum_replayed_row_id = standby
                .storage
                .scan_table(TABLE)?
                .iter()
                .filter_map(|row| row.row_id)
                .max()
                .ok_or("missing replayed row IDs")?;
        }
        previous = phase;
    }
    standby.execute(&format!(
        "INSERT INTO replay_perf VALUES ({}, 0, {})",
        n + 1,
        (n + 1) * 10
    ))?;
    let after_promotion = standby.storage.scan_table(TABLE)?;
    let counter_valid = after_promotion.len() == 1
        && after_promotion[0].row_id.is_some_and(|id| id > maximum_replayed_row_id)
        && numbers(&after_promotion[0])? == vec![n as i64 + 1, 0, (n as i64 + 1) * 10];
    all_valid &= counter_valid;
    report.record(json!({"kind":"counter_continuation", "valid":counter_valid,
        "maximum_replayed_row_id":maximum_replayed_row_id,
        "actual_row_ids":after_promotion.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        "actual_rows":after_promotion.iter().map(numbers).collect::<Outcome<Vec<_>>>()?}))?;
    Ok(all_valid)
}

fn run() -> Outcome<bool> {
    if cfg!(debug_assertions) {
        return Err("release build required".into());
    }
    let mut args = std::env::args().skip(1);
    let mut options = std::collections::HashMap::new();
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--output-dir" | "--label" | "--source-id" | "--rows" | "--storage" | "--round"
        ) {
            return Err(format!("unknown argument {key}").into());
        }
        let value = args.next().ok_or("each argument requires a value")?;
        if options.insert(key, value).is_some() {
            return Err("duplicate argument".into());
        }
    }
    let option = |key: &str| options.get(key).cloned().ok_or_else(|| format!("missing {key}"));
    let n: usize = option("--rows")?.parse()?;
    if n == 0 || n > 100_000 {
        return Err("rows must be in 1..=100000".into());
    }
    let storage = option("--storage")?;
    if storage != "disk" && storage != "memory" {
        return Err("storage must be disk or memory".into());
    }
    let round: usize = option("--round")?.parse()?;
    let output_dir = PathBuf::from(option("--output-dir")?);
    fs::create_dir_all(&output_dir)?;
    let root = tempfile::Builder::new()
        .prefix("live-replay-perf-")
        .tempdir_in(&output_dir)?
        .keep();
    let path = root.join("observations.jsonl");
    let mut report = Report {
        out: BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&path)?),
    };
    let executable = std::env::current_exe()?;
    let binary_sha256 = format!("{:x}", Sha256::digest(fs::read(&executable)?));
    report.record(
        json!({"kind":"metadata", "label":option("--label")?, "source_id":option("--source-id")?,
        "rows":n, "storage":storage, "round":round, "release":true, "pid":std::process::id(),
        "executable":executable, "executable_sha256":binary_sha256, "fixture_root":root,
        "default_storage_config_except":{"wal_enabled":true,"logical_wal_per_statement":true,
            "wal_sync_mode":"Async","wal_checkpoint_on_close":false,
            "wal_checkpoint_interval_entries":0,"wal_checkpoint_interval_secs":0},
        "indexes":["PRIMARY KEY(id)","UNIQUE(payload)","manual(grp)"],
        "retention":"disk mode retains both private datasets; memory mode datasets are internal temporary stores",
        "comparison_rule":"no ratio/performance acceptance against any failing baseline trial"}),
    )?;
    let result = trial(n, &storage, &root, &mut report);
    match &result {
        Ok(valid) => report.record(json!({"kind":"outcome", "passed":valid,
            "eligible_for_comparison":valid, "report":path}))?,
        Err(error) => report.record(json!({"kind":"outcome", "passed":false,
            "eligible_for_comparison":false,"error":error.to_string(),"report":path}))?,
    }
    report.out.get_ref().sync_all()?;
    println!("{}", json!({"report":path,"passed":matches!(&result,Ok(true))}));
    result
}
fn main() {
    match run() {
        Ok(true) => (),
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("live replay benchmark error: {error}");
            std::process::exit(2);
        }
    }
}
