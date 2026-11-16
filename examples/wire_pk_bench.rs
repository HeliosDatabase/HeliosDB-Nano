//! Primary-key write throughput over the PostgreSQL wire, against a server
//! that is already running.
//!
//! The embedded benchmarks (`benches/public`) call the engine directly, so
//! they cannot see what a statement costs between the socket and the engine —
//! which is where the per-statement handoff of `runtime_offload` sits. This
//! client measures that path with nothing but the dev-dependency
//! `tokio-postgres`: no `psql`, `pgbench` or Docker.
//!
//! Each client holds one connection and works on its own slice of the table,
//! so clients never wait for each other's rows:
//!
//! * `update` — `UPDATE pkbench SET n = $1 WHERE id = $2`;
//! * `delete-insert` — `DELETE FROM pkbench WHERE id = $1`, then the row's
//!   `INSERT` again (one operation is the pair);
//! * `select` — `SELECT n FROM pkbench WHERE id = $1`.
//!
//! `--protocol extended` prepares each statement once and binds per execution;
//! `--protocol simple` sends each statement as text.
//!
//! ```text
//! cargo run --release --example wire_pk_bench -- \
//!     --port 5432 --clients 16 --seconds 20 --mode update --label candidate
//! ```
//!
//! It prints one JSON object per run: throughput and the latency percentiles,
//! measured after `--warmup` seconds. `benches/public/wire_pk_ab.sh` runs it
//! against several server binaries in ABBA order.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stdout)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Update,
    DeleteInsert,
    Select,
}

#[derive(Clone)]
struct Args {
    host: String,
    port: u16,
    user: String,
    dbname: String,
    clients: usize,
    seconds: u64,
    warmup: u64,
    rows: usize,
    mode: Mode,
    extended: bool,
    setup: bool,
    label: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: wire_pk_bench --port N [--host H] [--user U] [--dbname D] [--clients N] [--seconds N] \
         [--warmup N] [--rows N] [--mode update|delete-insert|select] [--protocol extended|simple] \
         [--no-setup] [--label TEXT]"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = Args {
        host: "127.0.0.1".to_string(),
        port: 0,
        user: "postgres".to_string(),
        dbname: "postgres".to_string(),
        clients: 16,
        seconds: 20,
        warmup: 3,
        rows: 64_000,
        mode: Mode::Update,
        extended: true,
        setup: true,
        label: String::new(),
    };
    let mut words = std::env::args().skip(1);
    while let Some(flag) = words.next() {
        if flag == "--no-setup" {
            args.setup = false;
            continue;
        }
        let Some(value) = words.next() else { usage() };
        match flag.as_str() {
            "--host" => args.host = value,
            "--port" => args.port = value.parse().unwrap_or_else(|_| usage()),
            "--user" => args.user = value,
            "--dbname" => args.dbname = value,
            "--clients" => args.clients = value.parse().unwrap_or_else(|_| usage()),
            "--seconds" => args.seconds = value.parse().unwrap_or_else(|_| usage()),
            "--warmup" => args.warmup = value.parse().unwrap_or_else(|_| usage()),
            "--rows" => args.rows = value.parse().unwrap_or_else(|_| usage()),
            "--label" => args.label = value,
            "--mode" => {
                args.mode = match value.as_str() {
                    "update" => Mode::Update,
                    "delete-insert" => Mode::DeleteInsert,
                    "select" => Mode::Select,
                    _ => usage(),
                }
            }
            "--protocol" => {
                args.extended = match value.as_str() {
                    "extended" => true,
                    "simple" => false,
                    _ => usage(),
                }
            }
            _ => usage(),
        }
    }
    if args.port == 0 || args.clients == 0 || args.rows < args.clients || args.seconds == 0 {
        usage();
    }
    args
}

async fn connect(args: &Args) -> Client {
    let url = format!(
        "host={} port={} user={} dbname={} connect_timeout=5",
        args.host, args.port, args.user, args.dbname
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match tokio_postgres::connect(&url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                return client;
            }
            Err(error) if Instant::now() > deadline => panic!("no connection to {url}: {error}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

/// Create `pkbench` with ids `0..rows`, unless it already holds exactly that.
async fn setup(args: &Args) {
    let client = connect(args).await;
    client
        .batch_execute("CREATE TABLE IF NOT EXISTS pkbench (id INT PRIMARY KEY, n INT, v TEXT)")
        .await
        .expect("create pkbench");
    let have: i64 = client
        .query_one("SELECT count(*) FROM pkbench", &[])
        .await
        .expect("count pkbench")
        .get(0);
    if usize::try_from(have).ok() == Some(args.rows) {
        return;
    }
    client
        .batch_execute("DELETE FROM pkbench")
        .await
        .expect("empty pkbench");
    let mut id = 0;
    while id < args.rows {
        let end = (id + 500).min(args.rows);
        let values: Vec<String> = (id..end).map(|id| format!("({id}, 0, 'row-{id}')")).collect();
        client
            .batch_execute(&format!("INSERT INTO pkbench VALUES {}", values.join(", ")))
            .await
            .expect("populate pkbench");
        id = end;
    }
}

/// One client's loop over its own ids `first..first + span`. Returns the
/// latency, in microseconds, of every operation that began after `measure`
/// was raised, and the number of errors.
async fn run_client(
    args: Args,
    first: usize,
    span: usize,
    measure: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> (Vec<u32>, u64) {
    let client = connect(&args).await;
    let (update, delete, insert, select) = if args.extended {
        (
            Some(
                client
                    .prepare_typed("UPDATE pkbench SET n = $1 WHERE id = $2", &[Type::INT4, Type::INT4])
                    .await
                    .expect("prepare update"),
            ),
            Some(
                client
                    .prepare_typed("DELETE FROM pkbench WHERE id = $1", &[Type::INT4])
                    .await
                    .expect("prepare delete"),
            ),
            Some(
                client
                    .prepare_typed(
                        "INSERT INTO pkbench VALUES ($1, $2, $3)",
                        &[Type::INT4, Type::INT4, Type::TEXT],
                    )
                    .await
                    .expect("prepare insert"),
            ),
            Some(
                client
                    .prepare_typed("SELECT n FROM pkbench WHERE id = $1", &[Type::INT4])
                    .await
                    .expect("prepare select"),
            ),
        )
    } else {
        (None, None, None, None)
    };
    let mut latencies = Vec::with_capacity(1 << 20);
    let mut errors = 0_u64;
    let mut turn = 0_usize;
    while !stop.load(Ordering::Relaxed) {
        let id = i32::try_from(first + turn % span).expect("row id fits INT");
        let n = i32::try_from(turn % 1_000_000).expect("counter fits INT");
        turn += 1;
        let measured = measure.load(Ordering::Relaxed);
        let begun = Instant::now();
        let outcome = match (args.mode, args.extended) {
            (Mode::Update, true) => client
                .execute(update.as_ref().expect("prepared"), &[&n, &id])
                .await
                .map(|rows| rows == 1),
            (Mode::Update, false) => client
                .batch_execute(&format!("UPDATE pkbench SET n = {n} WHERE id = {id}"))
                .await
                .map(|()| true),
            (Mode::DeleteInsert, true) => match client.execute(delete.as_ref().expect("prepared"), &[&id]).await {
                Ok(deleted) => client
                    .execute(insert.as_ref().expect("prepared"), &[&id, &n, &"again"])
                    .await
                    .map(|inserted| deleted == 1 && inserted == 1),
                Err(error) => Err(error),
            },
            (Mode::DeleteInsert, false) => {
                match client
                    .batch_execute(&format!("DELETE FROM pkbench WHERE id = {id}"))
                    .await
                {
                    Ok(()) => client
                        .batch_execute(&format!("INSERT INTO pkbench VALUES ({id}, {n}, 'again')"))
                        .await
                        .map(|()| true),
                    Err(error) => Err(error),
                }
            }
            (Mode::Select, true) => client
                .query(select.as_ref().expect("prepared"), &[&id])
                .await
                .map(|rows| rows.len() == 1),
            (Mode::Select, false) => client
                .simple_query(&format!("SELECT n FROM pkbench WHERE id = {id}"))
                .await
                .map(|messages| messages.len() >= 2),
        };
        let micros = u32::try_from(begun.elapsed().as_micros()).unwrap_or(u32::MAX);
        match outcome {
            Ok(true) if measured => latencies.push(micros),
            Ok(true) => {}
            Ok(false) | Err(_) => errors += 1,
        }
    }
    (latencies, errors)
}

fn percentile(sorted: &[u32], per_mille: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "null".to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = parse_args();
    if args.setup {
        setup(&args).await;
    }
    let span = args.rows / args.clients;
    let measure = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let load_before = load_average();
    let clients: Vec<_> = (0..args.clients)
        .map(|client| {
            tokio::spawn(run_client(
                args.clone(),
                client * span,
                span,
                Arc::clone(&measure),
                Arc::clone(&stop),
            ))
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(args.warmup)).await;
    measure.store(true, Ordering::Relaxed);
    let begun = Instant::now();
    tokio::time::sleep(Duration::from_secs(args.seconds)).await;
    stop.store(true, Ordering::Relaxed);
    let elapsed = begun.elapsed().as_secs_f64();

    let mut latencies = Vec::new();
    let mut errors = 0;
    for client in clients {
        let (mut measured, failed) = client.await.expect("client task");
        latencies.append(&mut measured);
        errors += failed;
    }
    latencies.sort_unstable();
    let operations = latencies.len();
    let mean = if operations == 0 {
        0.0
    } else {
        latencies.iter().map(|&micros| f64::from(micros)).sum::<f64>() / operations as f64
    };
    let mode = match args.mode {
        Mode::Update => "update",
        Mode::DeleteInsert => "delete-insert",
        Mode::Select => "select",
    };
    println!(
        "{{\"label\":\"{}\",\"mode\":\"{mode}\",\"protocol\":\"{}\",\"clients\":{},\"rows\":{},\"seconds\":{elapsed:.3},\
         \"operations\":{operations},\"errors\":{errors},\"ops_per_sec\":{:.1},\"mean_us\":{mean:.1},\"p50_us\":{},\
         \"p95_us\":{},\"p99_us\":{},\"p999_us\":{},\"max_us\":{},\"load_before\":{load_before},\"load_after\":{}}}",
        args.label.replace(['"', '\\'], "_"),
        if args.extended { "extended" } else { "simple" },
        args.clients,
        args.rows,
        operations as f64 / elapsed,
        percentile(&latencies, 500),
        percentile(&latencies, 950),
        percentile(&latencies, 990),
        percentile(&latencies, 999),
        latencies.last().copied().unwrap_or(0),
        load_average(),
    );
}
