//! P0: real-daemon subprocess matrix for the HTTP auth / bind policy.
//!
//! These tests invoke the actual `heliosdb-nano` binary (`CARGO_BIN_EXE_…`) rather
//! than an in-process router, so they exercise `start_server`'s ordering: the HTTP
//! address is resolved and the exposure policy enforced BEFORE the database or HA
//! services start.
//!
//! The fail-fast cases each exit non-zero with a specific reason and never bind a
//! listener, so they are cheap and deterministic. The one positive case starts the
//! daemon with an authenticated PostgreSQL wire on `0.0.0.0` and proves the HTTP
//! listener still defaults to loopback by reaching `/health` on `127.0.0.1` — then
//! kills it.
//!
//! This file deliberately does not depend on any feature flag: every case uses the
//! `start` CLI surface that is always compiled.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_heliosdb-nano");

/// Reserve a free localhost port by binding and immediately releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Spawn the daemon with `args`, wait up to `timeout` for it to exit on its own,
/// then return `(success, combined stdout+stderr)`. A process that does not exit in
/// time is killed and the test fails — these cases are all supposed to fail closed
/// at startup.
fn run_until_exit(args: &[&str], timeout: Duration) -> (bool, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heliosdb-nano");

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let output = child.wait_with_output().expect("collect output");
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                return (status.success(), text);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("daemon did not exit within {timeout:?} for args {args:?}");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn assert_fail_closed(args: &[&str], needle: &str) {
    let (success, output) = run_until_exit(args, Duration::from_secs(30));
    assert!(!success, "daemon must exit non-zero for {args:?}; output:\n{output}");
    assert!(
        output.contains(needle),
        "output must contain {needle:?} for {args:?}; output:\n{output}"
    );
}

#[test]
fn trust_mode_remote_http_bind_is_refused_at_startup() {
    assert_fail_closed(
        &[
            "start",
            "--memory",
            "--auth",
            "trust",
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
        ],
        "trust mode",
    );
}

#[test]
fn authenticated_remote_http_bind_is_refused_even_with_allow_insecure() {
    assert_fail_closed(
        &[
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            "subprocess-secret",
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
            "--allow-insecure-http",
        ],
        "refusing to expose authenticated HTTP over plaintext",
    );
}

#[test]
fn trust_mode_with_explicit_password_still_refuses_remote_http() {
    assert_fail_closed(
        &[
            "start",
            "--memory",
            "--auth",
            "trust",
            "--password",
            "subprocess-secret",
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
            "--allow-insecure-http",
        ],
        "refusing to expose authenticated HTTP over plaintext",
    );
}

#[test]
fn md5_remote_http_bind_is_refused() {
    assert_fail_closed(
        &[
            "start",
            "--memory",
            "--auth",
            "md5",
            "--password",
            "subprocess-secret",
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
        ],
        "refusing to expose authenticated HTTP over plaintext",
    );
}

#[test]
fn mysql_with_authenticated_postgres_fails_closed_at_startup() {
    assert_fail_closed(
        &[
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            "subprocess-secret",
            "--port",
            "0",
            "--http-port",
            "0",
            "--mysql",
        ],
        "MySQL authentication is not implemented",
    );
}

#[test]
fn default_http_bind_is_loopback_when_database_listens_on_all_interfaces() {
    let pg_port = free_port();
    let http_port = free_port();

    let mut child = Command::new(BIN)
        .args([
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            "subprocess-secret",
            "--listen",
            "0.0.0.0",
            "--port",
            &pg_port.to_string(),
            "--http-port",
            &http_port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn heliosdb-nano");

    // The HTTP listener is expected on loopback, so connecting to 127.0.0.1 is the
    // proof: a daemon that inherited `--listen 0.0.0.0` would still answer here, but
    // this is the positive control that the default path starts at all.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        if let Ok(Some(status)) = child.try_wait() {
            let _ = child.kill();
            panic!("daemon exited during startup with {status}");
        }
        if let Ok(s) = TcpStream::connect(("127.0.0.1", http_port)) {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("HTTP listener never came up on 127.0.0.1:{http_port}");
        }
        thread::sleep(Duration::from_millis(100));
    };

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
        "health must answer 200; got:\n{response}"
    );
    assert!(
        response.contains("\"status\":\"ok\""),
        "health must be the fixed status-only body; got:\n{response}"
    );
}
