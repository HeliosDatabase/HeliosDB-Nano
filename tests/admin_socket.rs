#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]
//! The admin socket and the one rule for "may administer", against the real
//! server binary.
//!
//! * the socket appears in the data directory with mode 0600 and is removed
//!   at a clean shutdown; `--admin-socket` moves it or turns it off; a data
//!   directory writable by others gets none;
//! * the server's own user logs in on it as `postgres` without a password —
//!   also on a server that demands SCRAM everywhere else — and is an admin
//!   session; a role that is no superuser is refused server control there,
//!   a superuser role with a password is not; nothing on it is trusted;
//! * a session on plain TCP as `postgres` under trust is refused
//!   `SWITCHOVER`, `SWITCHOVER CHECK`, `pg_promote()` and `SET NODE ALIAS`
//!   with 42501, in both protocols; so is the ordinary Unix socket;
//! * a socket file left by a killed server is replaced;
//! * `status` reports from the running server, and says so;
//! * the ordinary sockets are 0770 unless told otherwise.
//!
//! Not here, and why: the refusal of a peer with another uid needs a second
//! operating-system user (the uid comparison is unit-tested in
//! `admin::peer`), and a TLS + SCRAM superuser session is covered by the
//! predicate's unit tests in `admin` (the rule reads only the listener, the
//! login method and the role, all of which those tests set directly).

#![cfg(unix)]

#[path = "support/ports.rs"]
mod ports;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_postgres::{NoTls, SimpleQueryMessage};

const ADMIN_SOCKET: &str = ".s.hdb.admin";
const NOT_ADMIN: &str = "42501";

struct Server {
    child: Option<Child>,
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    port: u16,
    log: PathBuf,
    password: Option<String>,
}

impl Server {
    /// A server that is not started yet: `<root>/data`, a private port.
    fn new(password: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            child: None,
            _dir: dir,
            data: root.join("data"),
            port: ports::free_port(),
            log: root.join("server.log"),
            root,
            password: password.map(str::to_string),
        }
    }

    /// `start --data-dir <root>/data` on a private port with `extra`.
    fn start(extra: &[&str], password: Option<&str>) -> Self {
        let mut server = Self::new(password);
        server.spawn(extra);
        server
    }

    /// Start (again) on the same data directory and port.
    fn spawn(&mut self, extra: &[&str]) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .arg("start")
            .arg("--data-dir")
            .arg(&self.data)
            .args(["--listen", "127.0.0.1", "--port", &self.port.to_string(), "--http-port", "0"])
            .args(extra)
            .env("RUST_LOG", "info")
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .env_remove("HELIOSDB_PASSWORD")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        match &self.password {
            Some(password) => {
                command
                    .args(["--auth", "scram-sha-256"])
                    .env("HELIOSDB_PASSWORD", password);
            }
            None => {
                command.args(["--auth", "trust"]);
            }
        }
        self.child = Some(command.spawn().unwrap());
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn admin_socket(&self) -> PathBuf {
        self.data.join(ADMIN_SOCKET)
    }

    /// A session on the PostgreSQL TCP port, waiting for the server to serve.
    async fn tcp(&self, user: &str) -> tokio_postgres::Client {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let mut config = tokio_postgres::Config::new();
            config
                .host("127.0.0.1")
                .port(self.port)
                .user(user)
                .dbname("postgres")
                .connect_timeout(Duration::from_secs(3));
            if let Some(password) = &self.password {
                config.password(password);
            }
            match config.connect(NoTls).await {
                Ok((client, connection)) => {
                    tokio::spawn(connection);
                    return client;
                }
                Err(error) if Instant::now() > deadline => {
                    panic!("no connection on {}: {error}; log: {}", self.port, self.log())
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }

    /// Wait until the server serves SQL on its TCP port.
    async fn ready(&self) {
        drop(self.tcp("postgres").await);
    }

    fn pid(&self) -> i32 {
        self.child.as_ref().unwrap().id() as i32
    }

    /// SIGTERM and wait: the clean shutdown path.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SAFETY: `kill` with a pid this test owns and a standard signal.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server did not stop on SIGTERM; log: {}", self.log());
        }
    }

    /// SIGKILL: no shutdown path runs.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

fn wait_for(what: &str, budget: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn is_socket(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

/// A session on the Unix socket at `socket` as `user`.
async fn unix_session(
    socket: &Path,
    user: &str,
    password: Option<&str>,
) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .unwrap_or_else(|error| panic!("{} does not accept connections: {error}", socket.display()));
    let mut config = tokio_postgres::Config::new();
    config
        .user(user)
        .dbname("postgres")
        .ssl_mode(tokio_postgres::config::SslMode::Disable);
    if let Some(password) = password {
        config.password(password);
    }
    let (client, connection) = config.connect_raw(stream, NoTls).await?;
    tokio::spawn(connection);
    Ok(client)
}

fn sqlstate(error: &tokio_postgres::Error) -> String {
    error.code().map(|code| code.code().to_string()).unwrap_or_default()
}

fn message(error: &tokio_postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
}

/// `(listener, auth_method)` of this session, from `pg_stat_activity`.
async fn arrival(client: &tokio_postgres::Client) -> (String, String) {
    let rows = client
        .simple_query("SELECT listener, auth_method FROM pg_stat_activity WHERE pid = pg_backend_pid()")
        .await
        .unwrap();
    rows.iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some((
                row.get(0).unwrap_or("NULL").to_string(),
                row.get(1).unwrap_or("NULL").to_string(),
            )),
            _ => None,
        })
        .expect("this session's row in pg_stat_activity")
}

/// The SQLSTATE each server-control statement answers on `client` (`ok` for
/// a statement that succeeded), in the simple protocol.
async fn control_answers(client: &tokio_postgres::Client) -> Vec<(&'static str, String, String)> {
    let mut answers = Vec::new();
    for sql in [
        "SWITCHOVER CHECK TO 'no-such-standby'",
        "SWITCHOVER TO 'no-such-standby'",
        "SELECT pg_promote(true, 5)",
        "SET NODE ALIAS 'spare' FOR 'no-such-node'",
    ] {
        let (code, text) = match client.simple_query(sql).await {
            Ok(_) => ("ok".to_string(), String::new()),
            Err(error) => (sqlstate(&error), message(&error)),
        };
        answers.push((sql, code, text));
    }
    answers
}

/// Every server-control statement is refused with 42501 on `client`, and
/// the refusal says why and how to connect as an administrator.
async fn assert_refused(client: &tokio_postgres::Client, why: &str) {
    for (sql, code, text) in control_answers(client).await {
        assert_eq!(code, NOT_ADMIN, "{sql}: {text}");
        assert!(text.contains("permission denied to run"), "{sql}: {text}");
        assert!(text.contains(why), "{sql}: the refusal says why ({why}): {text}");
        assert!(
            text.contains("admin socket") && text.contains("over TLS as a superuser role"),
            "{sql}: the refusal says how to connect as an administrator: {text}"
        );
    }
    // The extended protocol too: `pg_promote` is answered by the session there as well.
    let refused = client.query("SELECT pg_promote(true, 5)", &[]).await.unwrap_err();
    assert_eq!(sqlstate(&refused), NOT_ADMIN, "extended pg_promote: {refused}");
    // And the session is as usable as before.
    client.simple_query("SELECT 1").await.unwrap();
}

/// No server-control statement is refused for lack of privilege on `client`.
async fn assert_administers(client: &tokio_postgres::Client) {
    for (sql, code, text) in control_answers(client).await {
        assert_ne!(code, NOT_ADMIN, "{sql}: an admin session was refused: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_appears_with_mode_0600_and_a_clean_shutdown_removes_it() {
    let mut server = Server::start(&[], None);
    server.ready().await;
    let socket = server.admin_socket();
    assert!(is_socket(&socket), "no admin socket at {}; log: {}", socket.display(), server.log());
    assert_eq!(mode_of(&socket), 0o600, "the admin socket is its owner's only");
    server.stop();
    assert!(
        std::fs::symlink_metadata(socket).is_err(),
        "the admin socket outlived a clean shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_logs_in_without_a_password_and_is_an_admin_session() {
    // A server that demands SCRAM everywhere else.
    let server = Server::start(&[], Some("wire-secret"));
    server.ready().await;
    let admin = unix_session(&server.admin_socket(), "postgres", None)
        .await
        .unwrap_or_else(|error| panic!("the owner's peer login: {error}; log: {}", server.log()));
    assert_eq!(arrival(&admin).await, ("admin".to_string(), "peer".to_string()));
    assert_administers(&admin).await;
    // Every statement works on it: it is the ordinary handler.
    admin
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'one')")
        .await
        .unwrap();
    let rows = admin.query("SELECT v FROM t WHERE id = $1", &[&1_i32]).await.unwrap();
    assert_eq!(rows[0].get::<_, &str>(0), "one");

    // The same role with its password on plain TCP is not an administrator.
    let tcp = server.tcp("postgres").await;
    assert_eq!(arrival(&tcp).await, ("tcp".to_string(), "scram".to_string()));
    assert_refused(&tcp, "TCP connection without TLS").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_tcp_as_postgres_under_trust_is_refused_server_control() {
    let server = Server::start(&[], None);
    let tcp = server.tcp("postgres").await;
    assert_eq!(arrival(&tcp).await, ("tcp".to_string(), "trust".to_string()));
    assert_refused(&tcp, "TCP connection without TLS").await;
    // The owner on the admin socket of the same server is not refused.
    let admin = unix_session(&server.admin_socket(), "postgres", None).await.unwrap();
    assert_administers(&admin).await;
    // Both are listed for the operator.
    let listed = admin
        .simple_query("SELECT listener, auth_method FROM pg_stat_activity ORDER BY listener")
        .await
        .unwrap();
    let listed: Vec<(String, String)> = listed
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some((row.get(0)?.to_string(), row.get(1)?.to_string())),
            _ => None,
        })
        .collect();
    assert!(listed.contains(&("admin".to_string(), "peer".to_string())), "{listed:?}");
    assert!(listed.contains(&("tcp".to_string(), "trust".to_string())), "{listed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_that_is_no_superuser_is_refused_on_the_admin_socket() {
    let server = Server::start(&[], Some("wire-secret"));
    server.ready().await;
    let socket = server.admin_socket();
    let owner = unix_session(&socket, "postgres", None).await.unwrap();
    owner
        .batch_execute(
            "CREATE ROLE ops LOGIN PASSWORD 'ops-secret'; \
             CREATE ROLE boss WITH SUPERUSER LOGIN PASSWORD 'boss-secret'",
        )
        .await
        .unwrap();

    // Another role goes through the server's password method, also here.
    let wrong = unix_session(&socket, "ops", Some("not-it")).await.err().expect("a wrong password");
    assert_eq!(sqlstate(&wrong), "28P01", "{wrong}");
    let ops = unix_session(&socket, "ops", Some("ops-secret")).await.unwrap();
    assert_eq!(arrival(&ops).await, ("admin".to_string(), "scram".to_string()));
    assert_refused(&ops, "role \"ops\" is not a superuser").await;

    // A superuser role with its password is an administrator on the socket.
    let boss = unix_session(&socket, "boss", Some("boss-secret")).await.unwrap();
    assert_eq!(arrival(&boss).await, ("admin".to_string(), "scram".to_string()));
    assert_administers(&boss).await;
    // Until it is no superuser any more: the rule reads the role when asked.
    owner.batch_execute("ALTER ROLE boss WITH NOSUPERUSER").await.unwrap();
    assert_refused(&boss, "role \"boss\" is not a superuser").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_on_the_admin_socket_is_trusted() {
    // A trust server: TCP takes any name, the admin socket only the owner as
    // the built-in superuser.
    let server = Server::start(&[], None);
    server.ready().await;
    let refused = unix_session(&server.admin_socket(), "ops", None)
        .await
        .err()
        .expect("a role the socket would have to trust");
    assert_eq!(sqlstate(&refused), "28000", "{refused}");
    assert!(
        message(&refused).contains("does not authenticate by trust") && message(&refused).contains("\"postgres\""),
        "{}",
        message(&refused)
    );
    let owner = unix_session(&server.admin_socket(), "postgres", None).await.unwrap();
    assert_eq!(arrival(&owner).await, ("admin".to_string(), "peer".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_socket_file_is_replaced() {
    let mut server = Server::start(&[], None);
    server.ready().await;
    let socket = server.admin_socket();
    assert!(is_socket(&socket), "log: {}", server.log());
    // Killed: no shutdown path ran, the file stays and nothing listens on it.
    server.kill();
    assert!(is_socket(&socket), "a killed server leaves its socket file");
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_err());

    server.spawn(&[]);
    server.ready().await;
    let admin = unix_session(&socket, "postgres", None)
        .await
        .unwrap_or_else(|error| panic!("the replaced socket: {error}; log: {}", server.log()));
    assert_eq!(arrival(&admin).await.0, "admin");
    assert_eq!(mode_of(&socket), 0o600);
    assert!(server.log().contains("stale admin socket"), "log: {}", server.log());
}

fn status(server: &Server, json: bool) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command
        .arg("status")
        .arg("--data-dir")
        .arg(&server.data)
        .arg("--pid-file")
        .arg(server.root.join("no-such.pid"));
    if json {
        command.arg("--json");
    }
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_from_the_running_server_and_says_so() {
    let mut server = Server::start(&[], Some("wire-secret"));
    server.ready().await;
    // One session of our own, to be counted.
    let _session = server.tcp("postgres").await;

    let (code, text) = status(&server, true);
    assert_eq!(code, 0, "{text}");
    let report: serde_json::Value =
        serde_json::from_str(text.lines().next_back().unwrap_or_default()).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(report["running"], true, "{report}");
    assert_eq!(report["pid"], server.pid(), "{report}");
    assert_eq!(report["source"], "server", "{report}");
    assert_eq!(report["server"]["version"], env!("CARGO_PKG_VERSION"), "{report}");
    assert_eq!(report["server"]["role"], "primary", "{report}");
    assert_eq!(report["server"]["ready"], true, "{report}");
    assert!(report["server"]["epoch"].as_u64().is_some_and(|epoch| epoch >= 1), "{report}");
    // Ours, and the one `status` itself is.
    assert!(report["server"]["connections"].as_u64().is_some_and(|count| count >= 2), "{report}");

    let (code, text) = status(&server, false);
    assert_eq!(code, 0, "{text}");
    for want in [
        "Status: RUNNING",
        "Source: the running server (admin socket",
        concat!("Version: ", env!("CARGO_PKG_VERSION")),
        "Role: primary",
        "Ready: yes",
        "Connections:",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }

    // Stopped: the exit status it always had, and the files' answer.
    server.stop();
    let (code, text) = status(&server, true);
    assert_eq!(code, 3, "{text}");
    let report: serde_json::Value = serde_json::from_str(text.lines().next_back().unwrap_or_default()).unwrap();
    assert_eq!(report["running"], false, "{report}");
    assert_eq!(report["source"], "files", "{report}");
    assert!(report["server"].is_null(), "{report}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_falls_back_to_the_files_when_the_server_has_no_admin_socket() {
    let server = Server::start(&["--admin-socket", "off"], None);
    server.ready().await;
    assert!(std::fs::symlink_metadata(server.admin_socket()).is_err());
    let (code, text) = status(&server, true);
    assert_eq!(code, 0, "{text}");
    let report: serde_json::Value = serde_json::from_str(text.lines().next_back().unwrap_or_default()).unwrap();
    assert_eq!(report["running"], true, "{report}");
    assert_eq!(report["source"], "files", "{report}");
    assert!(
        report["server_unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("no server answers on the admin socket")),
        "{report}"
    );
    let (code, text) = status(&server, false);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Source: the PID file and the data directory"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_socket_can_be_moved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let moved = dir.path().join("admin.sock");
    let mut server = Server::start(&["--admin-socket", moved.to_str().unwrap()], None);
    server.ready().await;
    assert!(is_socket(&moved), "log: {}", server.log());
    assert_eq!(mode_of(&moved), 0o600);
    assert!(std::fs::symlink_metadata(server.admin_socket()).is_err(), "none in the data directory");
    let admin = unix_session(&moved, "postgres", None).await.unwrap();
    assert_eq!(arrival(&admin).await, ("admin".to_string(), "peer".to_string()));
    drop(admin);
    server.stop();
    assert!(std::fs::symlink_metadata(moved).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_data_directory_writable_by_others_gets_no_admin_socket() {
    let mut server = Server::new(None);
    std::fs::create_dir(&server.data).unwrap();
    std::fs::set_permissions(&server.data, std::fs::Permissions::from_mode(0o777)).unwrap();
    server.spawn(&[]);
    server.ready().await;
    assert!(
        std::fs::symlink_metadata(server.admin_socket()).is_err(),
        "an admin socket in a directory anyone can write"
    );
    let log = server.log();
    assert!(
        log.contains("the admin socket is not started") && log.contains("writable by others"),
        "the refusal is said: {log}"
    );
    // The server itself serves.
    server.tcp("postgres").await.simple_query("SELECT 1").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ordinary_sockets_are_0770_unless_told_otherwise() {
    let sockets = tempfile::tempdir().unwrap();
    let pg_dir = sockets.path().to_str().unwrap().to_string();
    let mysql = sockets.path().join("mysql.sock");
    let mut server = Server::start(
        &["--pg-socket-dir", &pg_dir, "--mysql-socket", mysql.to_str().unwrap()],
        None,
    );
    server.ready().await;
    let pg = sockets.path().join(format!(".s.PGSQL.{}", server.port));
    wait_for("the ordinary sockets", Duration::from_secs(20), || is_socket(&pg) && is_socket(&mysql));
    wait_for("their modes", Duration::from_secs(5), || mode_of(&pg) == 0o770 && mode_of(&mysql) == 0o770);

    // The ordinary PostgreSQL socket is not the admin socket.
    let local = unix_session(&pg, "postgres", None).await.unwrap();
    assert_eq!(arrival(&local).await, ("unix".to_string(), "trust".to_string()));
    assert_refused(&local, "which is not the admin socket").await;
    drop(local);

    server.stop();
    server.spawn(&[
        "--pg-socket-dir",
        &pg_dir,
        "--mysql-socket",
        mysql.to_str().unwrap(),
        "--pg-socket-mode",
        "0700",
        "--mysql-socket-mode",
        "660",
    ]);
    server.ready().await;
    wait_for("the ordinary sockets", Duration::from_secs(20), || is_socket(&pg) && is_socket(&mysql));
    wait_for("their modes", Duration::from_secs(5), || mode_of(&pg) == 0o700 && mode_of(&mysql) == 0o660);
}

#[test]
fn a_socket_mode_that_is_not_octal_permission_bits_refuses_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("start")
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .args(["--port", &ports::free_port().to_string(), "--http-port", "0"])
        .args(["--pg-socket-mode", "rwxrwx---"])
        .env_remove("HELIOSDB_PASSWORD")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("pg_socket_mode") && text.contains("octal"), "{text}");
    assert!(!dir.path().join("data").join("CURRENT").exists(), "refused before the database opened");
}
