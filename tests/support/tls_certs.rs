//! Throwaway certificates generated at run time with the `openssl` CLI, the
//! way `tests/mysql_ssl_tests.rs::generate_ca_and_signed_cert` does (an
//! explicit X.509v3 extension file: rustls's webpki refuses v1 certificates),
//! generalised to server certificates with subject alternative names.
//!
//! Included by path (`#[path = "support/tls_certs.rs"] mod tls_certs;`).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether the `openssl` CLI is available (the tests that need it skip with a
/// message otherwise).
pub fn openssl_available() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn openssl(args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute openssl: {e}"));
    assert!(
        output.status.success(),
        "openssl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn text(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// A self-signed CA: `(certificate, key)`, named after `name` so several CAs
/// can live in one directory.
pub fn ca(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let key = dir.join(format!("{name}-ca.key"));
    let cert = dir.join(format!("{name}-ca.crt"));
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        text(&key),
        "-out",
        text(&cert),
        "-days",
        "365",
        "-subj",
        &format!("/CN={name} test CA"),
    ]);
    (cert, key)
}

/// A leaf certificate for `cn` issued by `ca`: `(certificate, key)`. `sans`
/// are subject alternative names in openssl syntax (`IP:127.0.0.2`,
/// `DNS:localhost`); `usage` is the extended key usage (`serverAuth`,
/// `clientAuth`, or both comma-separated). The key is PKCS#8.
pub fn leaf(dir: &Path, ca: &(PathBuf, PathBuf), cn: &str, sans: &[&str], usage: &str) -> (PathBuf, PathBuf) {
    let key = dir.join(format!("{cn}.key"));
    let csr = dir.join(format!("{cn}.csr"));
    let cert = dir.join(format!("{cn}.crt"));
    let extensions = dir.join(format!("{cn}.ext"));
    openssl(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        text(&key),
        "-out",
        text(&csr),
        "-subj",
        &format!("/CN={cn}"),
    ]);
    let alternative_names = if sans.is_empty() {
        String::new()
    } else {
        format!("subjectAltName={}\n", sans.join(","))
    };
    std::fs::write(
        &extensions,
        format!(
            "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage={usage}\n\
             {alternative_names}"
        ),
    )
    .unwrap();
    openssl(&[
        "x509",
        "-req",
        "-in",
        text(&csr),
        "-CA",
        text(&ca.0),
        "-CAkey",
        text(&ca.1),
        "-CAcreateserial",
        "-out",
        text(&cert),
        "-days",
        "365",
        "-extfile",
        text(&extensions),
    ]);
    owner_only(&key);
    (cert, key)
}

/// Keys are owner-only, as an operator would keep them.
fn owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A CA, a server certificate for the replication port at `server_ip`, and a
/// client certificate for a standby, all under `dir`.
pub struct ReplicationPki {
    pub ca: (PathBuf, PathBuf),
    pub server: (PathBuf, PathBuf),
    pub client: (PathBuf, PathBuf),
}

pub fn replication_pki(dir: &Path, name: &str, server_ip: &str) -> ReplicationPki {
    let ca = ca(dir, name);
    let server = leaf(
        dir,
        &ca,
        &format!("{name}-server"),
        &[&format!("IP:{server_ip}"), "DNS:localhost"],
        "serverAuth",
    );
    let client = leaf(dir, &ca, &format!("{name}-standby"), &[], "clientAuth");
    ReplicationPki { ca, server, client }
}
