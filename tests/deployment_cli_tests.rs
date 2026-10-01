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
        let fixture = tempfile::tempdir().unwrap();
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
            return;
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
