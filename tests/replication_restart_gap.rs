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
    let (pg_p, repl_p, pg_s, repl_s) = (free_port(), free_port(), free_port(), free_port());
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
        "--replication-port".into(),
        repl_s.to_string(),
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
