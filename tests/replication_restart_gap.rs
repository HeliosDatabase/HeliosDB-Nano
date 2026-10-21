//! Standby restart gap (archive issue 2e98f330e009, 4.41.1 replication step 1).
//!
//! Real managed daemons, real wire: a primary and a standby are started, the
//! standby is stopped, the primary commits two more rows, the standby restarts,
//! and a further row is committed. The standby MUST end with every committed row.
//!
//! Observed on the 4.41.1 candidate before the fix: primary `[1,2,3,4]`, standby
//! `[1,4]`, the standby reporting healthy throughout. Rows committed while it was
//! down were silently lost, because reconnect resumed from live broadcast only and
//! the standby's applied position was not durable.
//!
//! A second case covers a standby that joins AFTER the data (and the DDL) exist:
//! it used to crash with `Table 't' does not exist`.
//!
//! Spec P1.3: the standby is now a PHYSICAL replica (bootstrapped from the
//! primary's replication port, which a primary with a replication token
//! serves), so every case here runs over the physical channel; the contracts
//! — no silent gap, every committed row, nothing uncommitted — are unchanged.
//! Spec P2.1: that channel is the only one — the standby's `--primary-host`
//! is the primary's `--replication-port`, on a private port per cluster. The
//! logical-only cases (the logical WAL as a standby's catch-up source, its
//! "full resync required" refusal) went with the logical standby; their
//! scenarios run here on the physical pair.

#![cfg(feature = "ha-tier1")]

use std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio_postgres::NoTls;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Kills exactly the child it owns when dropped, so a failed assertion cannot
/// leave a daemon holding ports or a data directory.
struct Daemon(Option<Child>);

impl Daemon {
    fn spawn(dir: &Path, name: &str, token: &Path, extra: &[String]) -> Self {
        let log = std::fs::File::create(dir.join(format!("{name}.log"))).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .arg("start")
            .args(["--data-dir"])
            .arg(dir.join(name))
            .args(["--listen", "127.0.0.1", "--auth", "trust", "--http-port", "0"])
            .arg("--replication-token-file")
            .arg(token)
            .args(extra)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        Self(Some(child))
    }

    /// Wait up to `budget` for the daemon to exit on its own; `None` if it is still running.
    fn wait_exit(&mut self, budget: Duration) -> Option<std::process::ExitStatus> {
        let child = self.0.as_mut()?;
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                self.0 = None;
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    /// Graceful stop (SIGTERM), then wait; a daemon that ignores it is killed.
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // SAFETY: `kill` with a pid we own and a standard signal.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn connect(port: u16) -> tokio_postgres::Client {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let url = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2");
        match tokio_postgres::connect(&url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                return client;
            }
            Err(e) if Instant::now() > deadline => panic!("no connection on {port}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    }
}

async fn ids(client: &tokio_postgres::Client) -> Result<Vec<i32>, String> {
    client
        .query("SELECT id FROM t ORDER BY id", &[])
        .await
        .map(|rows| rows.iter().map(|r| r.get::<_, i32>(0)).collect())
        .map_err(|e| e.to_string())
}

/// Poll until the standby shows `want` or the budget runs out; returns the last observation.
async fn settle(port: u16, want: &[i32], budget: Duration) -> Result<Vec<i32>, String> {
    let deadline = Instant::now() + budget;
    loop {
        let seen = match tokio_postgres::connect(
            &format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2"),
            NoTls,
        )
        .await
        {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                ids(&client).await
            }
            Err(e) => Err(e.to_string()),
        };
        if seen.as_deref() == Ok(want) || Instant::now() > deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct Cluster {
    dir: tempfile::TempDir,
    token: std::path::PathBuf,
    pg_p: u16,
    pg_s: u16,
    primary_args: Vec<String>,
    standby_args: Vec<String>,
}

fn cluster() -> Cluster {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("replication.token");
    std::fs::write(&token, [0x42u8; 32]).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (pg_p, repl_p, pg_s) = (free_port(), free_port(), free_port());
    let primary_args = vec![
        "--port".into(),
        pg_p.to_string(),
        "--replication-role".into(),
        "primary".into(),
        "--replication-port".into(),
        repl_p.to_string(),
    ];
    let standby_args = vec![
        "--port".into(),
        pg_s.to_string(),
        "--replication-role".into(),
        "standby".into(),
        "--primary-host".into(),
        format!("127.0.0.1:{repl_p}"),
    ];
    Cluster {
        dir,
        token,
        pg_p,
        pg_s,
        primary_args,
        standby_args,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standby_restart_recovers_rows_committed_while_down() {
    let c = cluster();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    let mut standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;

    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    assert_eq!(
        settle(c.pg_s, &[1], Duration::from_secs(20)).await,
        Ok(vec![1]),
        "live streaming must deliver the first row (precondition)"
    );

    standby.stop();
    writer.batch_execute("INSERT INTO t VALUES (2)").await.unwrap();
    writer.batch_execute("INSERT INTO t VALUES (3)").await.unwrap();

    let mut standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;
    writer.batch_execute("INSERT INTO t VALUES (4)").await.unwrap();

    let primary_rows = ids(&writer).await.unwrap();
    assert_eq!(primary_rows, vec![1, 2, 3, 4]);
    assert_eq!(
        settle(c.pg_s, &primary_rows, Duration::from_secs(30)).await,
        Ok(primary_rows.clone()),
        "standby must hold every committed row after restart, never a silent gap"
    );

    // A second restart must resume from the durable cursor: no duplicate replay,
    // no lost row, and a row committed in the gap still arrives.
    standby.stop();
    writer.batch_execute("INSERT INTO t VALUES (5)").await.unwrap();
    let _standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;
    assert_eq!(
        settle(c.pg_s, &[1, 2, 3, 4, 5], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3, 4, 5])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standby_joining_after_the_data_receives_ddl_and_rows() {
    let c = cluster();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer
        .batch_execute("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (3)")
        .await
        .unwrap();

    let _standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;
    assert_eq!(
        settle(c.pg_s, &[1, 2, 3], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3]),
        "history that predates the standby must be replayed, not crash the applier"
    );
}

/// Rolled-back and failed statements must never be shown to a standby: its rows must
/// equal the primary's committed rows, not the union of everything that was attempted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolled_back_and_failed_statements_never_reach_the_standby() {
    let c = cluster();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    let _standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;

    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    writer
        .batch_execute("BEGIN; INSERT INTO t VALUES (10); INSERT INTO t VALUES (11); ROLLBACK")
        .await
        .unwrap();
    // The third row collides with row 1: the whole statement must leave nothing behind.
    assert!(writer
        .batch_execute("INSERT INTO t VALUES (20), (21), (1)")
        .await
        .is_err());
    writer
        .batch_execute("BEGIN; INSERT INTO t VALUES (2); INSERT INTO t VALUES (3); COMMIT")
        .await
        .unwrap();

    let committed = ids(&writer).await.unwrap();
    assert_eq!(committed, vec![1, 2, 3]);
    assert_eq!(
        settle(c.pg_s, &committed, Duration::from_secs(30)).await,
        Ok(committed),
        "the standby must hold exactly what the primary committed"
    );
}

/// Field finding J (Sprinter 58d4e3113e56): restarting the PRIMARY used to break the
/// pair. Its clean close checkpointed the logical WAL and deleted every entry up to the
/// checkpoint, so a standby that was behind at that moment was refused for good and
/// exited. A physical standby continues from the primary's retained physical WAL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standby_catches_up_after_the_primary_restarts_while_it_is_down() {
    let c = cluster();
    let mut primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    let mut standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;

    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    assert_eq!(
        settle(c.pg_s, &[1], Duration::from_secs(20)).await,
        Ok(vec![1]),
        "precondition"
    );

    standby.stop();
    writer.batch_execute("INSERT INTO t VALUES (2)").await.unwrap();
    writer.batch_execute("INSERT INTO t VALUES (3)").await.unwrap();
    drop(writer);

    // Clean primary restart: the close-time checkpoint must not take the standby's
    // missing entries with it.
    primary.stop();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;

    let _standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    assert_eq!(
        settle(c.pg_s, &[1, 2, 3], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3]),
        "the standby must catch up after a primary restart instead of being refused"
    );
    writer.batch_execute("INSERT INTO t VALUES (4)").await.unwrap();
    assert_eq!(
        settle(c.pg_s, &[1, 2, 3, 4], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3, 4])
    );
}

/// Field finding J, second half: after a primary restart even a freshly wiped standby was
/// refused, so the pair could not be rebuilt in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_standby_after_a_primary_restart_receives_the_history() {
    let c = cluster();
    let mut primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer
        .batch_execute("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (3)")
        .await
        .unwrap();
    drop(writer);
    primary.stop();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let _ = connect(c.pg_p).await;

    let _standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    assert_eq!(
        settle(c.pg_s, &[1, 2, 3], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3]),
        "a new standby must receive history the primary still retains"
    );
}

/// `[replication] logical_wal_retention_entries` bounded what a restarted primary could
/// serve a LOGICAL standby: past it a fresh standby was refused for good.
/// Spec P1.3: a standby is physical, and the logical window bounds it no longer —
/// a standby one row behind still catches up after the primary restarts, and a NEW
/// standby is installed from a physical copy of the whole dataset instead of being
/// refused for history the logical window pruned. Spec P2.1: the window is only the
/// primary's own redo window (default 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logical_retention_window_no_longer_bounds_a_physical_standby() {
    let c = cluster();
    let config = c.dir.path().join("primary.toml");
    std::fs::write(&config, "[replication]\nlogical_wal_retention_entries = 12\n").unwrap();
    let mut primary_args = c.primary_args.clone();
    primary_args.extend(["--config".to_string(), config.display().to_string()]);

    let mut primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &primary_args);
    let writer = connect(c.pg_p).await;
    let mut standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let _ = connect(c.pg_s).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    let many: Vec<i32> = (1..=40).collect();
    for id in &many {
        writer
            .batch_execute(&format!("INSERT INTO t VALUES ({id})"))
            .await
            .unwrap();
    }
    assert_eq!(settle(c.pg_s, &many, Duration::from_secs(30)).await, Ok(many.clone()));

    // A standby one row behind is inside the window.
    standby.stop();
    writer.batch_execute("INSERT INTO t VALUES (41)").await.unwrap();
    drop(writer);
    primary.stop();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &primary_args);
    let _ = connect(c.pg_p).await;
    let mut standby = Daemon::spawn(c.dir.path(), "standby", &c.token, &c.standby_args);
    let all: Vec<i32> = (1..=41).collect();
    assert_eq!(
        settle(c.pg_s, &all, Duration::from_secs(30)).await,
        Ok(all),
        "a standby inside the retention window catches up after the primary restart"
    );
    standby.stop();

    // A NEW standby needs the whole dataset, whose logical history the primary pruned
    // past the window: it is installed from a physical copy, and holds every row.
    let port = free_port();
    let fresh_args = {
        let mut args = c.standby_args.clone();
        let at = args.iter().position(|a| a == "--port").unwrap();
        args[at + 1] = port.to_string();
        args
    };
    let mut fresh = Daemon::spawn(c.dir.path(), "fresh-standby", &c.token, &fresh_args);
    let all: Vec<i32> = (1..=41).collect();
    assert_eq!(
        settle(port, &all, Duration::from_secs(60)).await,
        Ok(all),
        "a fresh physical standby holds every row, whatever the logical window kept"
    );
    assert!(
        fresh.wait_exit(Duration::from_millis(100)).is_none(),
        "the fresh standby keeps running"
    );
}

/// Poll until the standby reports `want` rows or the budget runs out; returns the last observation.
async fn settle_count(port: u16, want: i64, budget: Duration) -> Result<i64, String> {
    let deadline = Instant::now() + budget;
    loop {
        let seen = match tokio_postgres::connect(
            &format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2"),
            NoTls,
        )
        .await
        {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                client
                    .query_one("SELECT count(*) FROM t", &[])
                    .await
                    .map(|row| row.get::<_, i64>(0))
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        if seen == Ok(want) || Instant::now() > deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A catch-up longer than one poll must arrive whole. Field-J crash probe on the J1
/// branch: a fresh (then logical) standby lost one whole batch on reconnect, then served
/// 2010 of 2510 rows and reported itself caught up. Three fresh standbys in turn
/// must each hold every row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_batch_catch_up_delivers_every_row() {
    const ROWS: i32 = 3000;
    let c = cluster();
    let _primary = Daemon::spawn(c.dir.path(), "primary", &c.token, &c.primary_args);
    let writer = connect(c.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    for id in 1..=ROWS {
        writer.execute("INSERT INTO t VALUES ($1)", &[&id]).await.unwrap();
    }
    for name in ["standby-a", "standby-b", "standby-c"] {
        let mut args = c.standby_args.clone();
        let port = free_port();
        let at = args.iter().position(|a| a == "--port").unwrap();
        args[at + 1] = port.to_string();
        let mut standby = Daemon::spawn(c.dir.path(), name, &c.token, &args);
        assert_eq!(
            settle_count(port, i64::from(ROWS), Duration::from_secs(60)).await,
            Ok(i64::from(ROWS)),
            "{name}: a fresh standby must hold every committed row"
        );
        standby.stop();
    }
}
