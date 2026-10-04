//! Real-binary regression tests for the PostgreSQL TCP `--require-tls` flag
//! (Issue 53).
//!
//! These launch the actual `heliosdb-nano` binary and assert that a
//! `start --require-tls` invocation missing a certificate or key is refused at
//! startup, before the database is opened or a daemon is spawned. They are
//! deliberately bounded: the process exits from argument validation, so no
//! listener is ever bound and no data directory is created.
//!
//! The positive path (a completed TLS handshake, exact SQL, and refusal of a
//! plaintext client while the same daemon remains healthy) is intentionally a
//! separate runtime gate; this file does not claim that the existing
//! `postgres_ssl_tests.rs` coverage proves it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_heliosdb-nano");

/// Run `args` and return `(success, combined output)`, failing if the process
/// has not exited within 30s (validation-only invocations are immediate).
fn run(args: &[&str]) -> (bool, String) {
    let stdout_path = std::env::temp_dir().join(format!(
        "heliosdb-require-tls-{}-{}.out",
        std::process::id(),
        args.join("_").replace(['/', '\\', ':'], "-")
    ));
    let stderr_path = stdout_path.with_extension("err");
    let stdout = std::fs::File::create(&stdout_path).expect("stdout log");
    let stderr = std::fs::File::create(&stderr_path).expect("stderr log");

    let mut child = Command::new(BIN)
        .args(args)
        .env_remove("HELIOSDB_PASSWORD")
        .env_remove("HELIOSDB_PASSWORD_FILE")
        .env_remove("HELIOSDB_MCP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn heliosdb-nano");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("heliosdb-nano did not exit within 30s for args {args:?}");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };

    let mut output = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    output.push_str(&std::fs::read_to_string(&stderr_path).unwrap_or_default());
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    (status.success(), output)
}

fn assert_require_tls_refused(args: &[&str], needle: &str) {
    let (success, output) = run(args);
    assert!(
        !success,
        "start must refuse {args:?} before opening the database, output:\n{output}"
    );
    assert!(
        output.contains(needle),
        "output must contain {needle:?} for {args:?}, output:\n{output}"
    );
}

#[test]
fn require_tls_without_cert_or_key_is_refused() {
    assert_require_tls_refused(
        &["start", "--memory", "--require-tls", "--port", "0", "--http-port", "0"],
        "--require-tls requires both --tls-cert and --tls-key",
    );
}

#[test]
fn require_tls_with_only_a_cert_is_refused() {
    assert_require_tls_refused(
        &[
            "start",
            "--memory",
            "--require-tls",
            "--tls-cert",
            "/nonexistent/cert.pem",
            "--port",
            "0",
            "--http-port",
            "0",
        ],
        "Both --tls-cert and --tls-key must be specified together",
    );
}

#[test]
fn require_tls_with_only_a_key_is_refused() {
    assert_require_tls_refused(
        &[
            "start",
            "--memory",
            "--require-tls",
            "--tls-key",
            "/nonexistent/key.pem",
            "--port",
            "0",
            "--http-port",
            "0",
        ],
        "Both --tls-cert and --tls-key must be specified together",
    );
}
