//! Strict receipts are emitted only after all timed work and independent oracles.
mod cells;
mod model;
mod oracles;

use crate::{ComparisonRow, PgClient};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub use oracles::{
    timed_execute_nano, timed_execute_pg, timed_show_nano, timed_show_pg, verify_category, verify_seed_data,
};

/// Common comparison shape: the preserved historical selective-index route
/// returned wrong correlated EXISTS rows. For seeded non-NULL INT ids1..200,
/// adding zero preserves the exact <=50 predicate and full50-name oracle.
/// Runtime admission of this generic shape is required on every comparison role.
pub const EXISTS_QUERY: &str =
    "SELECT name FROM customers WHERE EXISTS(SELECT 1 FROM orders WHERE customer_id=customers.id) AND id+0<=50";

pub const CATEGORIES: [&str; 35] = [
    "CREATE TABLE",
    "CREATE INDEX",
    "ALTER TABLE",
    "DROP TABLE",
    "CREATE/DROP VIEW",
    "REFRESH MATVIEW",
    "TRUNCATE",
    "INSERT single",
    "INSERT multi-row",
    "INSERT..SELECT",
    "UPDATE point",
    "DELETE point",
    "UPSERT",
    "UPDATE+subquery",
    "Point lookup",
    "Full scan+filter",
    "Aggregation",
    "INNER JOIN",
    "LEFT JOIN",
    "4-table JOIN",
    "Scalar subquery",
    "EXISTS subquery",
    "IN subquery",
    "CTE",
    "Recursive CTE",
    "Window funcs",
    "UNION",
    "DISTINCT",
    "ORDER+LIMIT",
    "CASE expr",
    "LIKE/BETWEEN/IN",
    "String ops",
    "Transaction ctl",
    "Prepared stmts",
    "SET/SHOW/RESET",
];
const PG_SETTINGS: [&str; 29] = [
    "jit",
    "work_mem",
    "synchronous_commit",
    "shared_buffers",
    "effective_cache_size",
    "max_parallel_workers_per_gather",
    "max_parallel_workers",
    "enable_hashjoin",
    "enable_mergejoin",
    "enable_nestloop",
    "enable_seqscan",
    "enable_indexscan",
    "enable_indexonlyscan",
    "enable_bitmapscan",
    "enable_material",
    "enable_memoize",
    "enable_partition_pruning",
    "random_page_cost",
    "seq_page_cost",
    "cpu_tuple_cost",
    "cpu_index_tuple_cost",
    "cpu_operator_cost",
    "default_statistics_target",
    "fsync",
    "full_page_writes",
    "wal_level",
    "plan_cache_mode",
    "geqo",
    "default_transaction_isolation",
];

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read required JSON artifact")).expect("valid required JSON artifact")
}
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("required receipt variable {name} is missing"))
}
fn absolute_env(name: &str) -> PathBuf {
    let path = PathBuf::from(env(name));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}
fn hash_file(path: &Path) -> String {
    digest(&fs::read(path).unwrap_or_else(|error| panic!("hash {}: {error}", path.display())))
}

/// Exactly these framed bytes are also written as harness-source.bundle by the
/// source-preparation script. Compile-time includes prevent a post-build source
/// path from masquerading as the source of the executable.
pub fn harness_source_hash() -> String {
    let files: [(&str, &[u8]); 5] = [
        ("tests/pg35_benchmark.rs", include_bytes!("../pg35_benchmark.rs")),
        ("tests/pg35_support/mod.rs", include_bytes!("mod.rs")),
        ("tests/pg35_support/cells.rs", include_bytes!("cells.rs")),
        ("tests/pg35_support/model.rs", include_bytes!("model.rs")),
        ("tests/pg35_support/oracles.rs", include_bytes!("oracles.rs")),
    ];
    let mut hash = Sha256::new();
    for (name, bytes) in files {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

pub struct ReceiptEnvironment {
    output: PathBuf,
    run: String,
    job: String,
    profile: Value,
    preflight: Value,
}
impl ReceiptEnvironment {
    pub fn from_env() -> Self {
        let output = absolute_env("PG35_STRICT_JSON");
        assert!(!output.exists(), "receipt path must be new");
        let run = env("PG35_RUN_ID");
        assert!(
            run.len() == 32
                && run
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "exact run ID required"
        );
        let job = env("PG35_JOB_ID");
        assert!(
            !job.is_empty()
                && job
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "exact job ID required"
        );
        let profile_path = absolute_env("PG35_PROFILE_JSON");
        let preflight_path = absolute_env("PG35_PREFLIGHT_JSON");
        let profile = read_json(&profile_path);
        let preflight = read_json(&preflight_path);
        assert_eq!(preflight["evidence_kind"], "executed_reference_preflight");
        assert_eq!(
            env("HELIOSDB_NANO_STATEMENT_OFFLOAD"),
            "on",
            "common offload policy cannot be disabled"
        );
        Self {
            output,
            run,
            job,
            profile,
            preflight,
        }
    }
}

pub struct RunContext {
    environment: ReceiptEnvironment,
    reference: Value,
    binary_sha256: String,
    source_sha256: String,
    runtime_config: Value,
    runtime_config_sha256: String,
    offload: Value,
    nano_work_mem: cells::Rows,
    pg_work_mem: cells::Rows,
}

#[allow(unexpected_cfgs)]
fn offload_identity() -> Value {
    #[cfg(pg35_has_statement_offload)]
    {
        heliosdb_nano::runtime_offload::set_statement_offload(true);
        assert!(heliosdb_nano::runtime_offload::statement_offload());
        json!({"available":true,"enabled":true,"environment_policy":"on"})
    }
    #[cfg(not(pg35_has_statement_offload))]
    {
        json!({"available":false,"enabled":null,"environment_policy":"on"})
    }
}

fn probe_reference(pg: &PgClient, connstr: &str, expected: &Value) -> Value {
    use tokio_postgres::config::Host;
    let config: tokio_postgres::Config = connstr.parse().expect("valid PG35 DSN");
    assert!(
        matches!(config.get_hosts(),[Host::Tcp(host)] if host=="127.0.0.1"),
        "documented loopback TCP endpoint required"
    );
    assert_eq!(config.get_ports(), [25433]);
    assert_eq!(config.get_dbname(), Some("benchdb"));
    assert_eq!(config.get_user(), Some("bench"));
    let rows=pg.rt.block_on(pg.client.query("SELECT current_setting('server_version_num')::int,version(),current_database(),inet_server_addr()::text,inet_server_port(),to_char(pg_postmaster_start_time() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",&[])).expect("probe actual PostgreSQL reference");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let version_num: i32 = row.try_get(0).unwrap();
    let version: String = row.try_get(1).unwrap();
    let database: String = row.try_get(2).unwrap();
    let address: String = row.try_get(3).unwrap();
    let port: i32 = row.try_get(4).unwrap();
    let started: String = row.try_get(5).unwrap();
    assert_eq!(version_num, 180004);
    assert!(version.starts_with("PostgreSQL 18.4"));
    assert_eq!(database, "benchdb");
    let control = pg
        .rt
        .block_on(
            pg.client
                .query("SELECT system_identifier::text FROM pg_control_system()", &[]),
        )
        .expect("probe PostgreSQL cluster system identifier");
    assert_eq!(control.len(), 1);
    let system: String = control[0].try_get(0).unwrap();
    assert!(!system.is_empty());
    let inputs: BTreeMap<String, String> = [
        ("database", database.clone()),
        ("endpoint", "127.0.0.1:25433".into()),
        ("postmaster_started_at", started),
        ("server_address", address),
        ("server_port", port.to_string()),
        ("system_identifier", system),
        ("version", version.clone()),
        ("version_num", version_num.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    let fingerprint = digest(&serde_json::to_vec(&inputs).unwrap());
    assert_eq!(expected["server_name"], "pg_bench_nano");
    assert_eq!(expected["version_num"], version_num);
    assert_eq!(expected["version"], version);
    assert_eq!(expected["database"], database);
    assert_eq!(expected["endpoint"], "127.0.0.1:25433");
    assert_eq!(
        expected["identity_inputs"],
        serde_json::to_value(&inputs).unwrap(),
        "PG instance/cluster/start time changed"
    );
    assert_eq!(expected["instance_id"], fingerprint);
    let wanted = PG_SETTINGS
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT name::text,setting::text FROM pg_settings WHERE name IN({wanted}) ORDER BY name");
    let settings = pg
        .rt
        .block_on(pg.client.query(&sql, &[]))
        .expect("probe native reference performance settings");
    assert_eq!(settings.len(), PG_SETTINGS.len());
    let settings: BTreeMap<String, String> = settings
        .iter()
        .map(|row| (row.try_get(0).unwrap(), row.try_get(1).unwrap()))
        .collect();
    assert_eq!(
        expected["settings"],
        serde_json::to_value(settings).unwrap(),
        "native PostgreSQL config changed"
    );
    expected.clone()
}

impl RunContext {
    pub fn new(db: &EmbeddedDatabase, pg: &PgClient, connstr: &str, environment: ReceiptEnvironment) -> Self {
        pg.execute("SET search_path=public")
            .expect("pin benchmark's declared schema");
        cells::assert_rows(
            cells::pg(pg, "SELECT current_schema()"),
            vec![vec![cells::text("public")]],
            true,
            "native benchmark schema",
        );
        let runtime_config = serde_json::to_value(db.storage.config()).expect("actual runtime config serializes");
        assert_eq!(
            runtime_config["storage"]["memory_only"], true,
            "preserve inherited embedded in-memory PG35 shape"
        );
        assert_eq!(
            runtime_config["storage"]["time_travel_enabled"], true,
            "time travel stays enabled on every product"
        );
        let runtime_config_sha256 = digest(&serde_json::to_vec(&runtime_config).unwrap());
        let offload = offload_identity();
        assert_eq!(
            environment.profile["pg35_offload_available"], offload["available"],
            "offload build cfg differs from source-proven profile"
        );
        let binary_sha256 = hash_file(&std::env::current_exe().expect("actual executable path"));
        let source_sha256 = harness_source_hash();
        assert_eq!(environment.profile["pg35_binary_sha256"], binary_sha256);
        assert_eq!(environment.profile["pg35_source_sha256"], source_sha256);
        let build_path = PathBuf::from(
            environment.profile["pg35_build_receipt"]
                .as_str()
                .expect("build receipt path"),
        );
        assert!(build_path.is_absolute());
        assert_eq!(environment.profile["pg35_build_receipt_sha256"], hash_file(&build_path));
        let build = read_json(&build_path);
        assert_eq!(build["binary_sha256"], binary_sha256);
        assert_eq!(build["harness_source_sha256"], source_sha256);
        for field in [
            "source_manifest_sha256",
            "commit",
            "rustc",
            "cargo",
            "features",
            "build_profile",
            "target",
        ] {
            assert_eq!(
                build[field], environment.profile[field],
                "build/profile mismatch {field}"
            );
        }
        assert_eq!(build["pg35_offload_available"], offload["available"]);
        assert_eq!(build["schema_version"], 1);
        assert_eq!(build["evidence_kind"], "build");
        assert_eq!(build["exit_code"], 0);
        assert!(build["command"].as_array().is_some_and(|command| !command.is_empty()));
        assert_eq!(
            build["build_environment"],
            environment.profile["pg35_build_environment"]
        );
        let reference = probe_reference(pg, connstr, &environment.preflight["reference"]);
        let nano_work_mem = cells::nano(db, "SHOW work_mem");
        let pg_work_mem = cells::pg(pg, "SHOW work_mem");
        Self {
            environment,
            reference,
            binary_sha256,
            source_sha256,
            runtime_config,
            runtime_config_sha256,
            offload,
            nano_work_mem,
            pg_work_mem,
        }
    }
}

pub fn finish_receipt(
    db: &EmbeddedDatabase,
    pg: &PgClient,
    context: &RunContext,
    results: &[ComparisonRow],
    dataset_counts: Value,
) {
    assert_eq!(results.len(), 35);
    for (row, name) in results.iter().zip(CATEGORIES) {
        assert_eq!(row.name, name);
        assert!(!row.helios_na);
        assert!(
            row.helios_avg_us.is_finite()
                && row.helios_avg_us > 0.0
                && row.pg_avg_us.is_finite()
                && row.pg_avg_us > 0.0
        );
        assert_eq!(row.proof["nano_successes"], 300);
        assert_eq!(row.proof["postgres_successes"], 300);
        assert_eq!(row.proof["nano_warmup_successes"], 2);
        assert_eq!(row.proof["postgres_warmup_successes"], 2);
        assert_eq!(row.proof["semantic_checks_passed"], true);
        assert!(row.proof["semantic_checks"].as_u64().unwrap() > 0);
    }
    assert_eq!(
        probe_reference(pg, &crate::pg35_connstr(), &context.reference),
        context.reference,
        "PG reference changed during run"
    );
    assert_eq!(
        serde_json::to_value(db.storage.config()).unwrap(),
        context.runtime_config,
        "Nano runtime config changed during run"
    );
    assert_eq!(
        hash_file(&std::env::current_exe().unwrap()),
        context.binary_sha256,
        "test binary changed during run"
    );
    cells::assert_rows(
        cells::pg(pg, "SELECT current_schema()"),
        vec![vec![cells::text("public")]],
        true,
        "native schema remained pinned",
    );
    let receipt = json!({"schema_version":1,"evidence_kind":"executed_pg35_strict_receipt","run_id":context.environment.run,"job_id":context.environment.job,
        "harness_source_sha256":context.source_sha256,"test_binary_sha256":context.binary_sha256,"iterations":300,"postgres_version_num":180004,
        "postgres_reference":context.reference,"validation":"strict_iterations_and_semantics","setup_success":true,"setup_errors":0,"dataset_counts":dataset_counts,
        "runtime_config":context.runtime_config,"runtime_config_sha256":context.runtime_config_sha256,"statement_offload":context.offload,
        "runtime_config_json":serde_json::to_string(&context.runtime_config).unwrap(),"pg_session_schema":"public",
        "crate_version":env!("CARGO_PKG_VERSION"),"profile_commit":context.environment.profile["commit"],"source_manifest_sha256":context.environment.profile["source_manifest_sha256"],
        "categories":results.iter().map(|row|row.proof.clone()).collect::<Vec<_>>()});
    let output = &context.environment.output;
    let parent = output.parent().expect("receipt parent");
    assert!(parent.is_dir());
    let temporary = parent.join(format!(
        ".pg35-{}-{}.partial",
        context.environment.job,
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .expect("new receipt staging file");
    file.write_all(&serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    fs::hard_link(&temporary, output).expect("publish complete new receipt without overwrite");
    fs::remove_file(&temporary).unwrap();
    fs::File::open(parent).unwrap().sync_all().unwrap();
}
