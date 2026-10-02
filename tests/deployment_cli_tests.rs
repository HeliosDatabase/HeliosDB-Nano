//! Process-level deployment regressions; all ports, data and processes are private.
//! Uses only the CLI and wire interfaces so the same source can run on the baseline.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const START_TIMEOUT: Duration = Duration::from_secs(15);

fn reserve(ip: &str) -> TcpListener {
    TcpListener::bind(format!("{ip}:0")).expect("reserve private port")
}

struct Server {
    child: Child,
    fixture: tempfile::TempDir,
    worker_pids: BTreeSet<u32>,
}

impl Server {
    fn start(pg_port: u16, args: &[&str], config: Option<&str>) -> Self {
        Self::start_with_existing_data(pg_port, args, config, false)
    }

    fn start_with_existing_data(pg_port: u16, args: &[&str], config: Option<&str>, existing: bool) -> Self {
        let fixture = tempfile::tempdir().unwrap();
        if existing {
            fs::create_dir(fixture.path().join("data")).unwrap();
            fs::write(fixture.path().join("data/sentinel"), b"existing unbound state").unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .current_dir(fixture.path())
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .env("RUST_LOG", "info")
            .args(["start", "--data-dir", "data", "--port", &pg_port.to_string()])
            .args(["--pid-file", "owned.pid"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(File::create(fixture.path().join("stdout.log")).unwrap())
            .stderr(File::create(fixture.path().join("stderr.log")).unwrap());
        if let Some(config) = config {
            fs::write(fixture.path().join("config.toml"), config).unwrap();
            command.args(["--config", "config.toml"]);
        }
        Self {
            child: command.spawn().unwrap(),
            fixture,
            worker_pids: BTreeSet::new(),
        }
    }

    fn logs(&self) -> String {
        ["stdout.log", "stderr.log"]
            .iter()
            .map(|name| fs::read_to_string(self.fixture.path().join(name)).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn remember_workers(&mut self) {
        let children = format!("/proc/{}/task/{}/children", self.child.id(), self.child.id());
        if let Ok(pids) = fs::read_to_string(children) {
            self.worker_pids
                .extend(pids.split_whitespace().filter_map(|pid| pid.parse::<u32>().ok()));
        }
        if let Ok(pid) = fs::read_to_string(self.fixture.path().join("owned.pid")) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                self.worker_pids.insert(pid);
            }
        }
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            self.remember_workers();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.remember_workers();
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit:\n{}", self.logs());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            self.remember_workers();
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "startup exited:\n{}",
                self.logs()
            );
            if self.logs().contains("Server ready!") {
                return;
            }
            assert!(Instant::now() < deadline, "startup not ready:\n{}", self.logs());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_failure(&mut self) {
        assert!(
            !self.wait_exit().success(),
            "unexpected startup success:\n{}",
            self.logs()
        );
        assert!(
            !self.logs().contains("Server ready!"),
            "false readiness:\n{}",
            self.logs()
        );
        assert!(
            !self.logs().contains("Daemon Started"),
            "false daemon readiness:\n{}",
            self.logs()
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.remember_workers();
        // Kill only recorded child PIDs whose current cwd still identifies this fixture.
        // This also handles assertions against a baseline that incorrectly starts a daemon.
        for pid in &self.worker_pids {
            if fs::read_link(format!("/proc/{pid}/cwd")).ok().as_deref() == Some(self.fixture.path()) {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn health(address: SocketAddr) {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("ok") || response.contains("healthy"), "{response}");
}

fn trust_session(port: u16) {
    let _ = trust_connection(port);
}

fn trust_connection(port: u16) -> TcpStream {
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let parameters = b"user\0postgres\0database\0postgres\0\0";
    let mut startup = ((parameters.len() + 8) as u32).to_be_bytes().to_vec();
    startup.extend_from_slice(&196608u32.to_be_bytes());
    startup.extend_from_slice(parameters);
    stream.write_all(&startup).unwrap();
    for _ in 0..64 {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
        assert!((4..=65536).contains(&size));
        let mut payload = vec![0; size - 4];
        stream.read_exact(&mut payload).unwrap();
        assert_ne!(header[0], b'E', "startup error: {}", String::from_utf8_lossy(&payload));
        if header[0] == b'R' {
            assert_eq!(payload, [0, 0, 0, 0], "trust unexpectedly challenged for credentials");
        }
        if header[0] == b'Z' {
            return stream;
        }
    }
    panic!("no ReadyForQuery received");
}

#[test]
fn http_socket_literal_serves_health() {
    let pg = reserve("127.0.0.1");
    let http = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let http_addr = http.local_addr().unwrap();
    drop((pg, http));
    let mut server = Server::start(pg_port, &["--http-listen", &http_addr.to_string()], None);
    server.wait_ready();
    health(http_addr);
}

#[test]
fn http_explicit_port_overrides_socket_suffix() {
    let pg = reserve("127.0.0.1");
    let suffix = reserve("127.0.0.1"); // Keep occupied: honoring suffix would fail.
    let http = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let http_addr = http.local_addr().unwrap();
    drop((pg, http));
    let mut server = Server::start(
        pg_port,
        &[
            "--http-listen",
            &suffix.local_addr().unwrap().to_string(),
            "--http-port",
            &http_addr.port().to_string(),
        ],
        None,
    );
    server.wait_ready();
    health(http_addr);
}

#[test]
fn http_ipv6_literal_serves_health_when_available() {
    let Ok(http) = TcpListener::bind("[::1]:0") else {
        eprintln!("IPv6 loopback is unavailable on this host");
        return;
    };
    let pg = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let http_addr = http.local_addr().unwrap();
    drop((pg, http));
    let mut server = Server::start(
        pg_port,
        &["--http-listen", "::1", "--http-port", &http_addr.port().to_string()],
        None,
    );
    server.wait_ready();
    health(http_addr);
}

#[test]
fn http_disabled_ignores_invalid_listen_address() {
    let pg = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    drop(pg);
    let mut server = Server::start(pg_port, &["--http-port", "0", "--http-listen", "not-an-address"], None);
    server.wait_ready();
    trust_session(pg_port);
}

#[test]
#[cfg(feature = "ha-tier1")]
fn replication_toml_starts_primary_listener() {
    let pg = reserve("127.0.0.1");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let replication_addr = replication.local_addr().unwrap();
    let config = format!(
        "[replication]\nrole='primary'\nreplication_port={}\nsync_mode='semi-sync'\n",
        replication_addr.port()
    );
    drop((pg, replication));
    let mut server = Server::start(pg_port, &["--http-port", "0"], Some(&config));
    server.wait_ready();
    assert!(server.logs().contains("Replication role: primary"), "{}", server.logs());
    TcpStream::connect_timeout(&replication_addr, Duration::from_secs(3)).unwrap();
    trust_session(pg_port);
}

#[test]
#[cfg(feature = "ha-tier1")]
fn replication_explicit_standalone_overrides_file_primary() {
    let pg = reserve("127.0.0.1");
    let replication = reserve("127.0.0.1"); // Primary startup would fail on this port.
    let pg_port = pg.local_addr().unwrap().port();
    let config = format!(
        "[replication]\nrole='primary'\nreplication_port={}\n",
        replication.local_addr().unwrap().port()
    );
    drop(pg);
    let mut server = Server::start(
        pg_port,
        &["--http-port", "0", "--replication-role", "standalone"],
        Some(&config),
    );
    server.wait_ready();
    assert!(!server.logs().contains("Replication role: primary"));
    trust_session(pg_port);
}

#[test]
#[cfg(feature = "ha-tier1")]
fn replication_cli_async_default_value_overrides_file_sync() {
    let pg = reserve("127.0.0.1");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let config = format!(
        "[replication]\nrole='primary'\nreplication_port={}\nsync_mode='sync'\n",
        replication.local_addr().unwrap().port()
    );
    drop((pg, replication));
    let mut server = Server::start(pg_port, &["--http-port", "0", "--sync-mode", "async"], Some(&config));
    server.wait_ready();
    assert!(server.logs().contains("Replication role: primary"), "{}", server.logs());
    assert!(server.logs().contains("Sync mode: async"), "{}", server.logs());
}

#[test]
fn replication_malformed_config_fails_before_readiness() {
    for fields in [
        "role='primray'",
        "replicaton_port=5433",
        "primary_host='http://localhost:5433'",
    ] {
        let pg = reserve("127.0.0.1");
        let pg_port = pg.local_addr().unwrap().port();
        drop(pg);
        let mut server = Server::start(
            pg_port,
            &["--http-port", "0"],
            Some(&format!("[replication]\n{fields}\n")),
        );
        server.assert_failure();
    }
}

#[test]
fn remote_trust_is_rejected_by_default() {
    let pg = reserve("0.0.0.0");
    let pg_port = pg.local_addr().unwrap().port();
    drop(pg);
    let mut server = Server::start(pg_port, &["--http-port", "0", "--listen", "0.0.0.0"], None);
    server.assert_failure();
    assert!(server.logs().contains("loopback"), "{}", server.logs());
}

#[test]
fn remote_trust_opt_in_serves_without_password_and_warns() {
    let pg = reserve("0.0.0.0");
    let pg_port = pg.local_addr().unwrap().port();
    drop(pg);
    let mut server = Server::start(
        pg_port,
        &["--http-port", "0", "--listen", "0.0.0.0", "--allow-insecure-trust"],
        None,
    );
    server.wait_ready();
    assert!(
        server.logs().contains("INSECURE DEVELOPMENT OVERRIDE"),
        "{}",
        server.logs()
    );
    trust_session(pg_port);
}

#[test]
fn remote_trust_opt_in_survives_daemon_reexec() {
    let pg = reserve("0.0.0.0");
    let pg_port = pg.local_addr().unwrap().port();
    drop(pg);
    let mut server = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--listen",
            "0.0.0.0",
            "--allow-insecure-trust",
            "--daemon",
        ],
        None,
    );
    assert!(server.wait_exit().success(), "{}", server.logs());
    assert!(
        server.logs().contains("INSECURE DEVELOPMENT OVERRIDE"),
        "{}",
        server.logs()
    );
    assert!(!server.worker_pids.is_empty(), "daemon PID missing");
    trust_session(pg_port);
}

#[test]
#[cfg(feature = "ha-tier1")]
fn occupied_replication_listener_fails_foreground_and_daemon() {
    for daemon in [false, true] {
        let pg = reserve("127.0.0.1");
        let replication = reserve("127.0.0.1");
        let pg_port = pg.local_addr().unwrap().port();
        let replication_port = replication.local_addr().unwrap().port().to_string();
        drop(pg);
        let mut args = vec![
            "--http-port",
            "0",
            "--replication-role",
            "primary",
            "--replication-port",
            &replication_port,
        ];
        if daemon {
            args.push("--daemon");
        }
        let mut server = Server::start(pg_port, &args, None);
        server.assert_failure();
        assert!(!server.fixture.path().join("owned.pid").exists());
        if !daemon {
            assert!(
                server.logs().contains("native replication listener"),
                "{}",
                server.logs()
            );
        }
    }
}

#[test]
fn unrelated_pg_listener_cannot_satisfy_daemon_readiness() {
    let unrelated = reserve("127.0.0.1");
    let mut server = Server::start(
        unrelated.local_addr().unwrap().port(),
        &["--http-port", "0", "--daemon"],
        None,
    );
    server.assert_failure();
    assert!(!server.fixture.path().join("owned.pid").exists());
}

#[test]
#[cfg(feature = "ha-tier1")]
fn guarded_standby_refuses_existing_unbound_data_before_database_open() {
    let pg = reserve("127.0.0.1");
    let port = pg.local_addr().unwrap().port();
    drop(pg);
    let mut server = Server::start_with_existing_data(
        port,
        &[
            "--http-port",
            "0",
            "--replication-role",
            "standby",
            "--primary-host",
            "127.0.0.1:1",
        ],
        None,
        true,
    );
    server.assert_failure();
    assert!(
        server.logs().contains("Cannot prepare standby history"),
        "{}",
        server.logs()
    );
    let data = server.fixture.path().join("data");
    assert_eq!(fs::read(data.join("sentinel")).unwrap(), b"existing unbound state");
    assert_eq!(
        fs::read_dir(data).unwrap().count(),
        1,
        "refusal must not open RocksDB or add metadata"
    );
}

#[test]
#[cfg(feature = "ha-tier1")]
fn guarded_standby_rejects_mysql_before_creating_data() {
    for extra in [vec!["--mysql"], vec!["--mysql-socket", "owned-mysql.sock"]] {
        let pg = reserve("127.0.0.1");
        let port = pg.local_addr().unwrap().port();
        drop(pg);
        let mut args = vec![
            "--http-port",
            "0",
            "--replication-role",
            "standby",
            "--primary-host",
            "127.0.0.1:1",
        ];
        args.extend(extra);
        let mut server = Server::start(port, &args, None);
        server.assert_failure();
        assert!(
            server.logs().contains("history-guarded standby does not support MySQL"),
            "{}",
            server.logs()
        );
        assert!(!server.fixture.path().join("data").exists());
        assert!(!server.fixture.path().join("owned-mysql.sock").exists());
    }
}

#[cfg(feature = "ha-tier1")]
fn simple_query_error(stream: &mut TcpStream, sql: &str) -> Option<String> {
    let mut query = vec![b'Q'];
    query.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
    query.extend_from_slice(sql.as_bytes());
    query.push(0);
    stream.write_all(&query).unwrap();
    let mut error = None;
    for _ in 0..128 {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        assert!((4..=65536).contains(&size));
        let mut payload = vec![0; size - 4];
        stream.read_exact(&mut payload).unwrap();
        if header[0] == b'E' {
            error = Some(String::from_utf8_lossy(&payload).into_owned());
        }
        if header[0] == b'Z' {
            return error;
        }
    }
    panic!("query did not finish");
}

#[test]
#[cfg(feature = "ha-tier1")]
fn guarded_standby_verifies_history_and_refuses_transparent_forwarding_in_every_mode() {
    let primary_pg = reserve("127.0.0.1");
    let primary_repl = reserve("127.0.0.1");
    let pg_port = primary_pg.local_addr().unwrap().port();
    let repl_port = primary_repl.local_addr().unwrap().port();
    drop((primary_pg, primary_repl));
    let mut primary = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--replication-role",
            "primary",
            "--replication-port",
            &repl_port.to_string(),
        ],
        None,
    );
    primary.wait_ready();
    let mut source = trust_connection(pg_port);
    assert!(simple_query_error(&mut source, "CREATE TABLE forwarding_guard (id INT PRIMARY KEY)").is_none());
    for mode in ["async", "semi-sync", "sync"] {
        let standby_pg = reserve("127.0.0.1");
        let standby_repl = reserve("127.0.0.1");
        let port = standby_pg.local_addr().unwrap().port();
        let native_port = standby_repl.local_addr().unwrap().port();
        drop((standby_pg, standby_repl));
        let mut standby = Server::start(
            port,
            &[
                "--http-port",
                "0",
                "--replication-role",
                "standby",
                "--replication-port",
                &native_port.to_string(),
                "--primary-host",
                &format!("127.0.0.1:{repl_port}"),
                "--sync-mode",
                mode,
            ],
            None,
        );
        standby.wait_ready();
        assert!(
            standby.logs().contains("Standby dataset history verified"),
            "{}",
            standby.logs()
        );
        let mut client = trust_connection(port);
        let error = simple_query_error(&mut client, "INSERT INTO forwarding_guard VALUES (1)")
            .expect("guarded standby must refuse transparent writes");
        // The write is refused by the guarded-standby admission classifier
        // (`statement is not permitted on a history-guarded standby`) before it
        // can reach the older forwarding-disabled path. Both refusals carry
        // SQLSTATE 25006 and the same "history-guarded standby" contract phrase,
        // which is what this test pins (it must not depend on which layer fires).
        assert!(
            error.contains("25006") && error.contains("history-guarded standby"),
            "{mode}: {error}"
        );
    }
    // The guarded requests never reached the source PG endpoint.
    assert!(simple_query_error(&mut source, "INSERT INTO forwarding_guard VALUES (1)").is_none());
}

#[test]
#[cfg(feature = "ha-tier1")]
fn replication_startup_timeout_cli_overrides_file_including_default_value() {
    for (configured, cli, expected) in [(7, None, 7), (2, Some("30"), 30)] {
        let pg = reserve("127.0.0.1");
        let native = reserve("127.0.0.1");
        let port = pg.local_addr().unwrap().port();
        let config = format!(
            "[replication]\nrole='primary'\nreplication_port={}\nstartup_timeout_secs={configured}\n",
            native.local_addr().unwrap().port()
        );
        drop((pg, native));
        let mut args = vec!["--http-port", "0"];
        if let Some(value) = cli {
            args.extend(["--replication-startup-timeout-secs", value]);
        }
        let mut server = Server::start(port, &args, Some(&config));
        server.wait_ready();
        assert!(
            server.logs().contains(&format!("History startup timeout: {expected}s")),
            "{}",
            server.logs()
        );
    }
}

#[test]
fn replication_startup_timeout_rejects_out_of_range_before_data_creation() {
    for value in ["0", "3601"] {
        for via_cli in [false, true] {
            let pg = reserve("127.0.0.1");
            let port = pg.local_addr().unwrap().port();
            drop(pg);
            let config = format!("[replication]\nstartup_timeout_secs={value}\n");
            let mut args = vec!["--http-port", "0"];
            if via_cli {
                args.extend(["--replication-startup-timeout-secs", value]);
            }
            let mut server = Server::start(port, &args, if via_cli { None } else { Some(&config) });
            server.assert_failure();
            assert!(
                server
                    .logs()
                    .contains("startup_timeout_secs must be between 1 and 3600"),
                "{}",
                server.logs()
            );
            assert!(!server.fixture.path().join("data").exists());
        }
    }
}

#[test]
#[cfg(feature = "ha-tier1")]
fn standby_small_timeout_bounds_verification_and_survives_daemon_reexec() {
    for daemon in [false, true] {
        // Own an idle endpoint that accepts TCP into its backlog but never sends
        // a handshake. This deterministically exercises the history startup budget.
        let peer = reserve("127.0.0.1");
        let pg = reserve("127.0.0.1");
        let port = pg.local_addr().unwrap().port();
        let endpoint = peer.local_addr().unwrap().to_string();
        drop(pg);
        let mut args = vec![
            "--http-port",
            "0",
            "--replication-role",
            "standby",
            "--primary-host",
            &endpoint,
            "--replication-startup-timeout-secs",
            "1",
        ];
        if daemon {
            args.push("--daemon");
        }
        // Without forwarding the explicit CLI override, the daemon child would
        // retain 45s and be killed by its parent, rather than fail its own 1s budget.
        let mut server = Server::start(port, &args, Some("[replication]\nstartup_timeout_secs=45\n"));
        server.assert_failure();
        if daemon {
            assert!(
                server.logs().contains("Daemon worker exited at startup"),
                "{}",
                server.logs()
            );
            assert!(
                !server.logs().contains("Daemon did not acknowledge"),
                "{}",
                server.logs()
            );
        } else {
            assert!(
                server
                    .logs()
                    .contains("did not verify primary dataset history within 1s"),
                "{}",
                server.logs()
            );
        }
    }
}

/// C3: an owner-only 32-byte token file for the plaintext-remote opt-in tests.
#[cfg(feature = "ha-tier1")]
fn replication_token_file() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replication.token");
    fs::write(&path, [0x5Au8; 32]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    (dir, path)
}

#[test]
#[cfg(feature = "ha-tier1")]
fn remote_replication_token_is_refused_without_opt_in() {
    let pg = reserve("0.0.0.0");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let replication_port = replication.local_addr().unwrap().port().to_string();
    let (_token_dir, token) = replication_token_file();
    let token_arg = token.display().to_string();
    drop((pg, replication));
    let mut server = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--listen",
            "0.0.0.0",
            "--allow-insecure-trust",
            "--replication-role",
            "primary",
            "--replication-port",
            &replication_port,
            "--replication-token-file",
            &token_arg,
        ],
        None,
    );
    server.assert_failure();
    assert!(
        server.logs().contains("allow-insecure-replication-remote"),
        "{}",
        server.logs()
    );
}

#[test]
#[cfg(feature = "ha-tier1")]
fn remote_replication_token_opt_in_serves_and_warns() {
    let pg = reserve("0.0.0.0");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let replication_port = replication.local_addr().unwrap().port().to_string();
    let (_token_dir, token) = replication_token_file();
    let token_arg = token.display().to_string();
    drop((pg, replication));
    let mut server = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--listen",
            "0.0.0.0",
            "--allow-insecure-trust",
            "--replication-role",
            "primary",
            "--replication-port",
            &replication_port,
            "--replication-token-file",
            &token_arg,
            "--allow-insecure-replication-remote",
        ],
        None,
    );
    server.wait_ready();
    assert!(
        server.logs().contains("INSECURE REPLICATION OVERRIDE"),
        "{}",
        server.logs()
    );
}

#[test]
#[cfg(feature = "ha-tier1")]
fn loopback_replication_token_is_unaffected() {
    let pg = reserve("127.0.0.1");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let replication_port = replication.local_addr().unwrap().port().to_string();
    let (_token_dir, token) = replication_token_file();
    let token_arg = token.display().to_string();
    drop((pg, replication));
    let mut server = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--listen",
            "127.0.0.1",
            "--replication-role",
            "primary",
            "--replication-port",
            &replication_port,
            "--replication-token-file",
            &token_arg,
        ],
        None,
    );
    server.wait_ready();
    assert!(
        !server.logs().contains("INSECURE REPLICATION OVERRIDE"),
        "loopback must not need the opt-in: {}",
        server.logs()
    );
}

#[test]
#[cfg(feature = "ha-tier1")]
fn remote_replication_token_opt_in_survives_daemon_reexec() {
    let pg = reserve("0.0.0.0");
    let replication = reserve("127.0.0.1");
    let pg_port = pg.local_addr().unwrap().port();
    let replication_port = replication.local_addr().unwrap().port().to_string();
    let (_token_dir, token) = replication_token_file();
    let token_arg = token.display().to_string();
    drop((pg, replication));
    let mut server = Server::start(
        pg_port,
        &[
            "--http-port",
            "0",
            "--listen",
            "0.0.0.0",
            "--allow-insecure-trust",
            "--replication-role",
            "primary",
            "--replication-port",
            &replication_port,
            "--replication-token-file",
            &token_arg,
            "--allow-insecure-replication-remote",
            "--daemon",
        ],
        None,
    );
    assert!(server.wait_exit().success(), "{}", server.logs());
    assert!(
        server.logs().contains("INSECURE REPLICATION OVERRIDE"),
        "{}",
        server.logs()
    );
    assert!(!server.worker_pids.is_empty(), "daemon PID missing");
}
