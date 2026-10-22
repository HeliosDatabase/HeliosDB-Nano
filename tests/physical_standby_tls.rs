//! Spec P2.2 §1, §4: remote replication over TLS on the one replication port.
//!
//! Real managed daemons (`tests/support/physical_pair.rs`) and certificates
//! generated at run time with the `openssl` CLI (`tests/support/tls_certs.rs`,
//! the `mysql_ssl_tests` recipe). The TLS pair runs on the loopback aliases
//! `127.0.0.2` (primary) and `127.0.0.3` (standby), routable on Linux without
//! setup, so the server certificate is verified against an IP subject
//! alternative name:
//!
//! * a TLS pair streams, and `/health`, `/ready` and `heliosdb_standby_status`
//!   say `tls: true` on both roles;
//! * mutual TLS: a standby without a client certificate is refused by the
//!   handshake (logged and counted by the primary) and keeps retrying; with
//!   one it streams;
//! * a standby whose CA is wrong is `reconnecting` with a `tls: …` reason —
//!   not `needs_recovery` — keeps serving its data within `max_standby_lag`,
//!   and recovers once its CA file is right again (the CA is read per
//!   connection attempt);
//! * plaintext to or from a non-loopback address is refused without
//!   `--allow-insecure-replication-remote` (both sides) and accepted with it,
//!   with an INSECURE warning;
//! * the token is still required over TLS (`Refuse { AuthFailed }`).
//!
//! Red on the base of this change (b46cdd44): the replication port was
//! loopback-only plaintext — none of the TLS flags existed, a non-loopback
//! bind or `--primary-host` was refused outright, and the opt-in changed
//! nothing.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;
#[path = "support/tls_certs.rs"]
mod tls_certs;

use physical_pair::*;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PRIMARY_HOST: &str = "127.0.0.2";
const STANDBY_HOST: &str = "127.0.0.3";

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// A pair's ports and token, and a PKI whose server certificate names
/// `127.0.0.2`.
struct TlsPair {
    pair: Pair,
    pki: tls_certs::ReplicationPki,
}

fn tls_pair() -> Option<TlsPair> {
    if !tls_certs::openssl_available() {
        eprintln!("skipped: the openssl CLI is not available");
        return None;
    }
    let pair = pair();
    let pki = tls_certs::replication_pki(pair.dir.path(), "replication", PRIMARY_HOST);
    Some(TlsPair { pair, pki })
}

impl TlsPair {
    fn dir(&self) -> &Path {
        self.pair.dir.path()
    }

    /// The primary on `127.0.0.2`, serving its replication port over TLS.
    fn spawn_primary(&self, name: &str, extra: &[String]) -> Daemon {
        let mut args = self.pair.primary_args.clone();
        args.extend(strings(&["--replication-tls", "--replication-tls-cert"]));
        args.push(text(&self.pki.server.0));
        args.push("--replication-tls-key".into());
        args.push(text(&self.pki.server.1));
        args.extend(extra.iter().cloned());
        Daemon::spawn_with(
            self.dir(),
            name,
            &self.dir().join("primary"),
            &self.pair.token,
            &args,
            None,
            &SpawnOptions {
                listen: Some(PRIMARY_HOST),
                ..SpawnOptions::default()
            },
        )
    }

    /// A standby on `127.0.0.3` following `127.0.0.2` over TLS, trusting `ca`.
    fn spawn_standby(&self, name: &str, token: &Path, ca: &Path, extra: &[String], config: Option<&str>) -> Daemon {
        let mut args = vec![
            "--port".to_string(),
            self.pair.pg_s.to_string(),
            "--replication-role".into(),
            "standby".into(),
            "--primary-host".into(),
            format!("{PRIMARY_HOST}:{}", self.pair.repl_p),
            "--http-port".into(),
            self.pair.http_s.to_string(),
            "--replication-startup-timeout-secs".into(),
            "30".into(),
            "--primary-tls".into(),
            "--primary-ca".into(),
            text(ca),
        ];
        args.extend(extra.iter().cloned());
        Daemon::spawn_with(
            self.dir(),
            name,
            &self.dir().join(name),
            token,
            &args,
            config,
            &SpawnOptions {
                listen: Some(STANDBY_HOST),
                ..SpawnOptions::default()
            },
        )
    }
}

/// Poll `http_port`'s `/health` replication block until `want` holds.
async fn wait_block(
    http_port: u16,
    daemon: &Daemon,
    budget: Duration,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        if let Some((_, body)) = http(http_port, "/health").await {
            let block = body["replication"].clone();
            if want(&block) {
                return block;
            }
            assert!(
                Instant::now() < deadline,
                "never {what}: {block}; log tail: {}",
                daemon.tail(12)
            );
        } else {
            assert!(Instant::now() < deadline, "no /health: {}", daemon.tail(12));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn create_table(port_host: &str, port: u16, rows: &str) -> tokio_postgres::Client {
    let writer = connect_to(port_host, port).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    writer
        .batch_execute(&format!("INSERT INTO t VALUES {rows}"))
        .await
        .unwrap();
    writer
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tls_pair_streams_and_both_roles_report_tls() {
    let Some(t) = tls_pair() else { return };
    let primary = t.spawn_primary("primary", &[]);
    let writer = create_table(PRIMARY_HOST, t.pair.pg_p, "(1, 'one'), (2, 'two')").await;
    let standby = t.spawn_standby("standby", &t.pair.token, &t.pki.ca.0, &[], None);
    let ready = wait_http(t.pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    assert_eq!(ready["tls"], true, "the standby's session is on TLS: {ready}");
    assert!(ready["reason"].is_null(), "{ready}");
    let health = wait_http(t.pair.http_p, "/health", 200, Duration::from_secs(30), &primary).await;
    assert_eq!(health["replication"]["tls"], true, "{health}");
    assert_eq!(
        health["replication"]["primary"],
        format!("{PRIMARY_HOST}:{}", t.pair.repl_p),
        "the replication port listens on --listen's (loopback alias) address: {health}"
    );
    writer.batch_execute("INSERT INTO t VALUES (3, 'three')").await.unwrap();
    assert_eq!(
        settle_on(STANDBY_HOST, t.pair.pg_s, &[1, 2, 3], Duration::from_secs(60)).await,
        Ok(vec![1, 2, 3])
    );
    let reader = connect_to(STANDBY_HOST, t.pair.pg_s).await;
    assert_eq!(
        value(&reader, "SELECT tls FROM heliosdb_standby_status")
            .await
            .as_deref(),
        Some("t")
    );
    assert!(
        primary.log_contains_within("(NPHYS002, TLS", Duration::from_secs(10)),
        "{}",
        primary.tail(12)
    );
    assert!(
        standby.log_contains_within(", TLS)", Duration::from_secs(10)),
        "the standby logs its session's transport: {}",
        standby.tail(12)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_tls_refuses_a_standby_without_a_certificate_and_serves_one_with_it() {
    let Some(t) = tls_pair() else { return };
    let primary = t.spawn_primary("primary", &strings(&["--replication-tls-ca", &text(&t.pki.ca.0)]));
    let writer = create_table(PRIMARY_HOST, t.pair.pg_p, "(1, 'one')").await;

    // No client certificate: the handshake is refused; the standby retries.
    let mut anonymous = t.spawn_standby("anonymous", &t.pair.token, &t.pki.ca.0, &[], None);
    let block = wait_block(
        t.pair.http_s,
        &anonymous,
        Duration::from_secs(60),
        "a connection failure reason",
        |block| block["reason"].is_string(),
    )
    .await;
    assert_ne!(block["state"], "needs_recovery", "a TLS refusal is retried: {block}");
    assert_eq!(block["ready"], false, "{block}");
    let reason = block["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("tls: ") || reason.contains("I/O"),
        "the reason is the transport's: {block}"
    );
    assert!(
        primary.log_contains_within("replication TLS handshake from", Duration::from_secs(30)),
        "the primary logs and counts the refused handshake: {}",
        primary.tail(12)
    );
    assert!(anonymous.alive());
    anonymous.stop();

    // With one: it streams; the token is checked as well (it is the pair's).
    let standby = t.spawn_standby(
        "standby",
        &t.pair.token,
        &t.pki.ca.0,
        &[
            "--primary-client-cert".into(),
            text(&t.pki.client.0),
            "--primary-client-key".into(),
            text(&t.pki.client.1),
        ],
        None,
    );
    let ready = wait_http(t.pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(ready["tls"], true, "{ready}");
    writer.batch_execute("INSERT INTO t VALUES (2, 'two')").await.unwrap();
    assert_eq!(
        settle_on(STANDBY_HOST, t.pair.pg_s, &[1, 2], Duration::from_secs(60)).await,
        Ok(vec![1, 2])
    );
    assert!(primary.log_contains_within("client certificates required", Duration::from_secs(5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_ca_is_a_reconnect_with_a_tls_reason_and_recovers_with_the_right_one() {
    let Some(t) = tls_pair() else { return };
    let ca_file: PathBuf = t.dir().join("standby-trusts.pem");
    std::fs::copy(&t.pki.ca.0, &ca_file).unwrap();
    let wrong = tls_certs::ca(t.dir(), "wrong");

    let mut primary = t.spawn_primary("primary-1", &[]);
    create_table(PRIMARY_HOST, t.pair.pg_p, "(1, 'one'), (2, 'two')").await;
    let standby = t.spawn_standby(
        "standby",
        &t.pair.token,
        &ca_file,
        &[],
        Some("[replication]\nmax_standby_lag = \"300s\"\n"),
    );
    wait_http(t.pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(
        settle_on(STANDBY_HOST, t.pair.pg_s, &[1, 2], Duration::from_secs(60)).await,
        Ok(vec![1, 2])
    );

    // The standby's trust changes to another CA, and the primary restarts:
    // every new connection fails certificate verification.
    std::fs::copy(&wrong.0, &ca_file).unwrap();
    primary.stop();
    let primary = t.spawn_primary("primary-2", &[]);
    let block = wait_block(
        t.pair.http_s,
        &standby,
        Duration::from_secs(90),
        "reconnecting with a tls reason",
        |block| block["state"] == "reconnecting" && block["reason"].as_str().is_some_and(|r| r.starts_with("tls: ")),
    )
    .await;
    assert_eq!(
        block["ready"], true,
        "within max_standby_lag the data is still served: {block}"
    );
    assert!(
        standby.log().contains("the TLS connection to the physical source"),
        "{}",
        standby.tail(12)
    );
    // The data is intact and served.
    assert_eq!(
        settle_on(STANDBY_HOST, t.pair.pg_s, &[1, 2], Duration::from_secs(10)).await,
        Ok(vec![1, 2])
    );
    assert!(primary.log_contains_within("replication TLS handshake from", Duration::from_secs(30)));

    // The right CA again: the next attempt reads it and the standby streams.
    std::fs::copy(&t.pki.ca.0, &ca_file).unwrap();
    let block = wait_block(
        t.pair.http_s,
        &standby,
        Duration::from_secs(90),
        "streaming again",
        |block| block["state"] == "streaming",
    )
    .await;
    assert_eq!(block["tls"], true, "{block}");
    assert!(
        block["reason"].is_null(),
        "a delivering session clears the reason: {block}"
    );
    let writer = connect_to(PRIMARY_HOST, t.pair.pg_p).await;
    writer.batch_execute("INSERT INTO t VALUES (3, 'three')").await.unwrap();
    assert_eq!(
        settle_on(STANDBY_HOST, t.pair.pg_s, &[1, 2, 3], Duration::from_secs(60)).await,
        Ok(vec![1, 2, 3])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_is_still_required_over_tls() {
    let Some(t) = tls_pair() else { return };
    let primary = t.spawn_primary("primary", &[]);
    create_table(PRIMARY_HOST, t.pair.pg_p, "(1, 'one')").await;
    let wrong = token_file(t.dir(), "wrong.token", 0x24);
    let standby = t.spawn_standby("standby", &wrong, &t.pki.ca.0, &[], None);
    let block = wait_block(
        t.pair.http_s,
        &standby,
        Duration::from_secs(90),
        "needs_recovery",
        |block| block["state"] == "needs_recovery",
    )
    .await;
    assert!(
        block["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("authentication failed")),
        "Refuse {{ AuthFailed }} over a verified TLS session: {block}"
    );
    assert!(
        primary.log_contains_within("authentication failed", Duration::from_secs(10)),
        "{}",
        primary.tail(12)
    );
}

/// Both sides refuse plaintext over a non-loopback address without the
/// opt-in and accept it with one, warning. `0.0.0.0` is a non-loopback
/// address to the rule (a wildcard bind exposes every interface) and reaches
/// the local host when connected to, so no second host is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_over_the_network_needs_the_insecure_opt_in_on_both_sides() {
    let p = pair();
    let wildcard = SpawnOptions {
        listen: Some("0.0.0.0"),
        ..SpawnOptions::default()
    };
    let mut primary_args = p.primary_args.clone();
    primary_args.push("--allow-insecure-trust".into());

    // A standby refuses a plaintext non-loopback primary at start.
    let mut refused = Daemon::spawn(
        p.dir.path(),
        "refused-standby",
        &p.dir.path().join("refused-standby"),
        &p.token,
        &[
            "--port".to_string(),
            p.pg_s.to_string(),
            "--replication-role".into(),
            "standby".into(),
            "--primary-host".into(),
            format!("0.0.0.0:{}", p.repl_p),
            "--primary-tls=false".into(),
            "--http-port".into(),
            p.http_s.to_string(),
        ],
        None,
    );
    let status = refused
        .wait_exit(Duration::from_secs(30))
        .unwrap_or_else(|| panic!("a plaintext remote primary was accepted: {}", refused.tail(12)));
    assert!(!status.success());
    let log = refused.log();
    assert!(
        log.contains(&format!("resolves to 0.0.0.0:{}", p.repl_p))
            && log.contains("--allow-insecure-replication-remote"),
        "{log}"
    );

    // A primary on a wildcard --listen keeps a plaintext replication port on loopback.
    let mut primary = Daemon::spawn_with(
        p.dir.path(),
        "loopback-primary",
        &p.dir.path().join("primary"),
        &p.token,
        &primary_args,
        None,
        &wildcard,
    );
    let health = wait_http(p.http_p, "/health", 200, Duration::from_secs(30), &primary).await;
    assert_eq!(
        health["replication"]["primary"],
        format!("127.0.0.1:{}", p.repl_p),
        "{health}"
    );
    assert_eq!(health["replication"]["tls"], false);
    assert!(primary.log_contains_within("stays on loopback", Duration::from_secs(10)));
    primary.stop();

    // With the opt-in on both sides: plaintext over the network, warned about.
    let mut insecure_args = primary_args.clone();
    insecure_args.push("--allow-insecure-replication-remote".into());
    let primary = Daemon::spawn_with(
        p.dir.path(),
        "insecure-primary",
        &p.dir.path().join("primary"),
        &p.token,
        &insecure_args,
        None,
        &wildcard,
    );
    let health = wait_http(p.http_p, "/health", 200, Duration::from_secs(30), &primary).await;
    assert_eq!(
        health["replication"]["primary"],
        format!("0.0.0.0:{}", p.repl_p),
        "{health}"
    );
    assert!(primary.log_contains_within("INSECURE DEVELOPMENT OVERRIDE", Duration::from_secs(10)));
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1), (2)")
        .await
        .unwrap();
    let standby = Daemon::spawn(
        p.dir.path(),
        "insecure-standby",
        &p.dir.path().join("standby"),
        &p.token,
        &[
            "--port".to_string(),
            p.pg_s.to_string(),
            "--replication-role".into(),
            "standby".into(),
            "--primary-host".into(),
            format!("0.0.0.0:{}", p.repl_p),
            "--primary-tls=false".into(),
            "--allow-insecure-replication-remote".into(),
            "--http-port".into(),
            p.http_s.to_string(),
            "--replication-startup-timeout-secs".into(),
            "30".into(),
        ],
        None,
    );
    let ready = wait_http(p.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(ready["tls"], false, "{ready}");
    assert!(standby.log_contains_within("INSECURE DEVELOPMENT OVERRIDE", Duration::from_secs(10)));
    assert_eq!(settle(p.pg_s, &[1, 2], Duration::from_secs(60)).await, Ok(vec![1, 2]));
}

/// The rule itself, on non-loopback addresses (no daemon, no socket).
#[test]
fn the_non_loopback_rule_on_a_remote_address() {
    use heliosdb_nano::replication::physical_tls::{remote_transport_allowed, ReplicationTransport};
    let remote: std::net::SocketAddr = "198.51.100.7:5433".parse().unwrap();
    assert!(!remote_transport_allowed(&remote, false, false));
    assert!(remote_transport_allowed(&remote, true, false));
    assert!(remote_transport_allowed(&remote, false, true));
    assert!(!ReplicationTransport::default().allows(&remote));
    assert!(ReplicationTransport::default().allows(&"127.0.0.2:5433".parse().unwrap()));
}
