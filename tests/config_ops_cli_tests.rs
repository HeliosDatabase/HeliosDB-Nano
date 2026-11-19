//! Process-level regressions for config-file handling of `start` (v03.1 listen
//! address and port, v03.4 unknown and inert keys, strict mode) and for
//! `config check` (v03.5). Every port is loopback and reserved, every data
//! directory is a scratch directory.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![cfg(target_os = "linux")]

use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "support/ports.rs"]
mod ports;

const TIMEOUT: Duration = Duration::from_secs(20);

struct Proc {
    child: Child,
    dir: tempfile::TempDir,
}

impl Proc {
    /// `heliosdb-nano start <args> [--config config.toml]` in a scratch directory.
    fn start(args: &[&str], config: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .current_dir(dir.path())
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .env("RUST_LOG", "info")
            .arg("start")
            .args(args)
            .stdin(Stdio::null())
            .stdout(File::create(dir.path().join("stdout.log")).unwrap())
            .stderr(File::create(dir.path().join("stderr.log")).unwrap());
        if let Some(config) = config {
            fs::write(dir.path().join("config.toml"), config).unwrap();
            command.args(["--config", "config.toml"]);
        }
        Self {
            child: command.spawn().unwrap(),
            dir,
        }
    }

    fn logs(&self) -> String {
        ["stdout.log", "stderr.log"]
            .iter()
            .map(|name| fs::read_to_string(self.dir.path().join(name)).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
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

    fn wait_failure(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(!status.success(), "unexpected success:\n{}", self.logs());
                assert!(
                    !self.logs().contains("Server ready!"),
                    "false readiness:\n{}",
                    self.logs()
                );
                return;
            }
            assert!(Instant::now() < deadline, "process did not exit:\n{}", self.logs());
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn trust_session(address: SocketAddr) {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let parameters = b"user\0postgres\0database\0postgres\0\0";
    let mut startup = ((parameters.len() + 8) as u32).to_be_bytes().to_vec();
    startup.extend_from_slice(&196608u32.to_be_bytes());
    startup.extend_from_slice(parameters);
    stream.write_all(&startup).unwrap();
    for _ in 0..64 {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0; size - 4];
        stream.read_exact(&mut payload).unwrap();
        assert_ne!(header[0], b'E', "startup error: {}", String::from_utf8_lossy(&payload));
        if header[0] == b'Z' {
            return;
        }
    }
    panic!("no ReadyForQuery received");
}

// ---------------------------------------------------------------- v03.1

#[test]
fn file_only_port_is_bound() {
    let port = ports::free_port();
    let mut server = Proc::start(
        &["--memory", "--http-port", "0"],
        Some(&format!("[server]\nport = {port}\n")),
    );
    server.wait_ready();
    let logs = server.logs();
    assert!(logs.contains(&format!("Listen address: 127.0.0.1:{port}")), "{logs}");
    assert!(logs.contains("port: config file"), "{logs}");
    trust_session(SocketAddr::from(([127, 0, 0, 1], port)));
}

#[test]
fn flag_port_beats_file_port_even_when_it_is_the_old_default() {
    let port = ports::free_port();
    let other = ports::free_port();
    let mut server = Proc::start(
        &["--memory", "--http-port", "0", "--port", &port.to_string()],
        Some(&format!("[server]\nport = {other}\n")),
    );
    server.wait_ready();
    let logs = server.logs();
    assert!(logs.contains(&format!("Listen address: 127.0.0.1:{port}")), "{logs}");
    assert!(logs.contains("port: --port"), "{logs}");
    trust_session(SocketAddr::from(([127, 0, 0, 1], port)));
}

#[test]
fn file_listen_addr_is_bound_and_flag_beats_it() {
    let port = ports::free_port();
    let mut server = Proc::start(
        &["--memory", "--http-port", "0", "--port", &port.to_string()],
        Some("[server]\nlisten_addr = \"127.0.0.2\"\n"),
    );
    server.wait_ready();
    let logs = server.logs();
    assert!(logs.contains(&format!("Listen address: 127.0.0.2:{port}")), "{logs}");
    assert!(logs.contains("listen: config file"), "{logs}");
    trust_session(SocketAddr::from(([127, 0, 0, 2], port)));
    drop(server);

    let port = ports::free_port();
    let mut server = Proc::start(
        &[
            "--memory",
            "--http-port",
            "0",
            "--port",
            &port.to_string(),
            "--listen",
            "127.0.0.1",
        ],
        Some("[server]\nlisten_addr = \"127.0.0.2\"\n"),
    );
    server.wait_ready();
    let logs = server.logs();
    assert!(logs.contains(&format!("Listen address: 127.0.0.1:{port}")), "{logs}");
    assert!(logs.contains("listen: --listen"), "{logs}");
}

#[test]
fn file_ipv6_listen_addr_is_bound() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("no IPv6 loopback on this host; skipped");
        return;
    }
    let port = ports::free_port();
    let mut server = Proc::start(
        &["--memory", "--http-port", "0", "--port", &port.to_string()],
        Some("[server]\nlisten_addr = \"::1\"\n"),
    );
    server.wait_ready();
    trust_session(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)));
}

#[test]
fn file_non_loopback_listen_under_trust_is_refused() {
    let port = ports::free_port();
    let mut server = Proc::start(
        &["--memory", "--http-port", "0", "--port", &port.to_string()],
        Some("[server]\nlisten_addr = \"0.0.0.0\"\n"),
    );
    server.wait_failure();
    assert!(server.logs().contains("loopback"), "{}", server.logs());
}

#[test]
fn daemon_honours_file_port_and_parent_banner_names_it() {
    let port = ports::free_port();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), format!("[server]\nport = {port}\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .current_dir(dir.path())
        .args([
            "start",
            "--data-dir",
            "data",
            "--http-port",
            "0",
            "--config",
            "config.toml",
            "--pid-file",
            "d.pid",
            "--daemon",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).to_string() + &String::from_utf8_lossy(&output.stderr);
    let pid: i32 = fs::read_to_string(dir.path().join("d.pid"))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    let outcome = std::panic::catch_unwind(|| {
        assert!(output.status.success(), "{text}");
        assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
        trust_session(SocketAddr::from(([127, 0, 0, 1], port)));
    });
    if pid > 0 {
        // SAFETY: signalling the daemon this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    outcome.unwrap();
}

// ---------------------------------------------------------------- v03.4

#[test]
fn stray_key_warns_once_and_strict_mode_refuses() {
    let port = ports::free_port();
    let config = "[authentication]\npasswrod = \"x\"\n";
    let mut server = Proc::start(
        &["--memory", "--http-port", "0", "--port", &port.to_string()],
        Some(config),
    );
    server.wait_ready();
    let logs = server.logs();
    assert_eq!(logs.matches("passwrod").count(), 1, "{logs}");
    drop(server);

    let port = ports::free_port();
    let mut strict = Proc::start(
        &[
            "--memory",
            "--http-port",
            "0",
            "--port",
            &port.to_string(),
            "--strict-config",
        ],
        Some(config),
    );
    strict.wait_failure();
    assert!(strict.logs().contains("passwrod"), "{}", strict.logs());
}

// ---------------------------------------------------------------- v03.5

struct Check {
    code: i32,
    stdout: String,
    stderr: String,
}

fn config_check(content: &str, extra: &[&str]) -> Check {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");
    fs::write(&path, content).unwrap();
    config_check_file(&path, extra)
}

fn config_check_file(path: &std::path::Path, extra: &[&str]) -> Check {
    let output = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["config", "check", "--config"])
        .arg(path)
        .args(extra)
        .output()
        .unwrap();
    Check {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

#[test]
fn check_example_file_is_clean_and_prints_no_secret() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let result = config_check_file(&example, &["--strict"]);
    assert_eq!(result.code, 0, "{}\n{}", result.stdout, result.stderr);

    let seeded = config_check(
        "[api]\njwt_secret = \"SECRET_API_S3\"\n[authentication]\njwt_secret = \"SECRET_AUTH_S4\"\n",
        &[],
    );
    assert_eq!(seeded.code, 0, "{}\n{}", seeded.stdout, seeded.stderr);
    for text in [&seeded.stdout, &seeded.stderr] {
        assert!(!text.contains("SECRET_API_S3"), "{text}");
        assert!(!text.contains("SECRET_AUTH_S4"), "{text}");
    }
    // the inert key is named, in its own list
    assert!(seeded.stdout.contains("jwt_secret"), "{}", seeded.stdout);
}

#[test]
fn check_unknown_key_warns_and_strict_exits_two() {
    let content = "[authentication]\npasswrod = \"x\"\n";
    let lenient = config_check(content, &[]);
    assert_eq!(lenient.code, 0, "{}\n{}", lenient.stdout, lenient.stderr);
    assert!(lenient.stdout.contains("passwrod"), "{}", lenient.stdout);
    let strict = config_check(content, &["--strict"]);
    assert_eq!(strict.code, 2, "{}\n{}", strict.stdout, strict.stderr);
}

#[test]
fn check_validation_failure_exits_one_naming_the_key() {
    let result = config_check("[server]\nmax_connections_warn_percent = 101\n", &[]);
    assert_eq!(result.code, 1, "{}\n{}", result.stdout, result.stderr);
    assert!(
        format!("{}{}", result.stdout, result.stderr).contains("max_connections_warn_percent"),
        "{}\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn check_json_parses_and_carries_warnings_array() {
    let result = config_check("[authentication]\npasswrod = \"x\"\n", &["--json"]);
    assert_eq!(result.code, 0, "{}\n{}", result.stdout, result.stderr);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert!(value["warnings"].as_array().is_some_and(|w| !w.is_empty()), "{value}");
    assert!(value["errors"].as_array().is_some(), "{value}");
}
