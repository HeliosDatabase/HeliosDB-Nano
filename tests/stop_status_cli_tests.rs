//! v03.6: `stop` waits for the shutdown and signals only a verified server;
//! `status` reports through its exit code and JSON. Loopback ports, scratch
//! directories, private processes only.
//!
//! Exit codes (chosen on purpose, documented in the help text): `stop` 0
//! stopped, 1 failed or still shutting down, 3 not running, 4 stale PID file
//! (the process is not the server; nothing was signalled). `status` 0
//! running, 3 not running, 4 stale PID file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![cfg(target_os = "linux")]

use std::{
    fs,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "support/ports.rs"]
mod ports;

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.env_remove("HELIOSDB_NANO_READY_FILE");
    command
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    let probe = unsafe { libc::kill(pid, 0) };
    if probe != 0 {
        return false;
    }
    // A zombie child of this test is not alive.
    !fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z "))
}

fn wait_gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

struct Daemon {
    dir: tempfile::TempDir,
    pid: i32,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Self { dir, pid: 0 };
        daemon.spawn()
    }

    fn spawn(mut self) -> Self {
        let port = ports::free_port();
        let output = bin()
            .current_dir(self.dir.path())
            .args([
                "start",
                "--data-dir",
                "data",
                "--port",
                &port.to_string(),
                "--http-port",
                "0",
                "--pid-file",
                "d.pid",
                "--daemon",
            ])
            .output()
            .unwrap();
        self.pid = fs::read_to_string(self.dir.path().join("d.pid"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        assert!(
            output.status.success() && self.pid > 0,
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        bin().current_dir(self.dir.path()).args(args).output().unwrap()
    }

    fn pid_file(&self) -> std::path::PathBuf {
        self.dir.path().join("d.pid")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.pid > 0 {
            // SAFETY: the daemon this test started.
            unsafe {
                libc::kill(self.pid, libc::SIGCONT);
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn stop_waits_and_leaves_a_process_that_is_still_shutting_down() {
    let daemon = Daemon::start();
    // SAFETY: stopping the daemon this test started; SIGTERM stays pending.
    unsafe { libc::kill(daemon.pid, libc::SIGSTOP) };
    let started = Instant::now();
    let stop = daemon.run(&["stop", "--pid-file", "d.pid", "--data-dir", "data", "--timeout", "1"]);
    assert_eq!(stop.status.code(), Some(1), "{}", text(&stop));
    assert!(text(&stop).contains("still shutting down"), "{}", text(&stop));
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(
        alive(daemon.pid),
        "stop killed a server that was not asked to be killed"
    );
    assert!(
        daemon.pid_file().exists(),
        "the PID file must stay while the server runs"
    );

    // SAFETY: resuming the daemon this test started; it now shuts down cleanly.
    unsafe { libc::kill(daemon.pid, libc::SIGCONT) };
    assert!(wait_gone(daemon.pid, Duration::from_secs(30)), "daemon did not exit");
    // a second stop finds nothing running and says so
    let again = daemon.run(&["stop", "--pid-file", "d.pid", "--data-dir", "data"]);
    assert_eq!(again.status.code(), Some(3), "{}", text(&again));
    assert!(!daemon.pid_file().exists());
    // the directory reopens: a new daemon starts on it
    let again = daemon.run(&[
        "start",
        "--data-dir",
        "data",
        "--port",
        &ports::free_port().to_string(),
        "--http-port",
        "0",
        "--pid-file",
        "d.pid",
        "--daemon",
    ]);
    assert!(again.status.success(), "{}", text(&again));
    let new_pid: i32 = fs::read_to_string(daemon.pid_file()).unwrap().trim().parse().unwrap();
    assert_ne!(new_pid, daemon.pid);
    let _ = daemon.run(&["stop", "--pid-file", "d.pid", "--data-dir", "data"]);
}

#[test]
fn stop_force_kills_after_the_timeout() {
    let daemon = Daemon::start();
    // SAFETY: stopping the daemon this test started.
    unsafe { libc::kill(daemon.pid, libc::SIGSTOP) };
    let stop = daemon.run(&[
        "stop",
        "--pid-file",
        "d.pid",
        "--data-dir",
        "data",
        "--timeout",
        "1",
        "--force",
    ]);
    assert_eq!(stop.status.code(), Some(0), "{}", text(&stop));
    assert!(wait_gone(daemon.pid, Duration::from_secs(10)));
    assert!(!daemon.pid_file().exists());
}

#[test]
fn stop_with_a_stale_pid_file_never_signals_the_unrelated_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut sleeper = Command::new("sleep").arg("60").stdout(Stdio::null()).spawn().unwrap();
    let pid_file = dir.path().join("stale.pid");
    fs::write(&pid_file, sleeper.id().to_string()).unwrap();

    let stop = bin().args(["stop", "--pid-file"]).arg(&pid_file).output().unwrap();
    assert_eq!(stop.status.code(), Some(4), "{}", text(&stop));
    assert!(alive(sleeper.id() as i32), "stop signalled an unrelated process");
    assert!(!pid_file.exists(), "the stale PID file must be removed");

    fs::write(&pid_file, sleeper.id().to_string()).unwrap();
    let status = bin().args(["status", "--pid-file"]).arg(&pid_file).output().unwrap();
    assert_eq!(status.status.code(), Some(4), "{}", text(&status));
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn daemon_start_replaces_a_stale_pid_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut sleeper = Command::new("sleep").arg("60").stdout(Stdio::null()).spawn().unwrap();
    fs::write(dir.path().join("d.pid"), sleeper.id().to_string()).unwrap();
    let daemon = Daemon { dir, pid: 0 }.spawn();
    assert_ne!(daemon.pid, sleeper.id() as i32);
    assert!(alive(sleeper.id() as i32));
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn stop_by_data_dir_alone_and_not_running_exit_code() {
    let daemon = Daemon::start();
    // no PID file given: the process holding the directory's LOCK is the server
    let stop = daemon.run(&["stop", "--data-dir", "data", "--pid-file", "missing.pid"]);
    assert_eq!(stop.status.code(), Some(0), "{}", text(&stop));
    assert!(wait_gone(daemon.pid, Duration::from_secs(30)));
    let idle = daemon.run(&["stop", "--data-dir", "data", "--pid-file", "missing.pid"]);
    assert_eq!(idle.status.code(), Some(3), "{}", text(&idle));
}

#[test]
fn status_exit_codes_and_json() {
    let daemon = Daemon::start();
    let running = daemon.run(&["status", "--pid-file", "d.pid"]);
    assert_eq!(running.status.code(), Some(0), "{}", text(&running));

    let json = daemon.run(&["status", "--pid-file", "d.pid", "--json"]);
    assert_eq!(json.status.code(), Some(0), "{}", text(&json));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["running"], true);
    assert_eq!(value["pid"], daemon.pid);
    for key in ["state", "pid_file", "started"] {
        assert!(value.get(key).is_some(), "missing {key}: {value}");
    }

    let stop = daemon.run(&["stop", "--pid-file", "d.pid", "--data-dir", "data"]);
    assert_eq!(stop.status.code(), Some(0), "{}", text(&stop));
    let none = daemon.run(&["status", "--pid-file", "d.pid"]);
    assert_eq!(none.status.code(), Some(3), "{}", text(&none));
    let none_json = daemon.run(&["status", "--pid-file", "d.pid", "--json"]);
    assert_eq!(none_json.status.code(), Some(3));
    let value: serde_json::Value = serde_json::from_slice(&none_json.stdout).unwrap();
    assert_eq!(value["running"], false);
}
