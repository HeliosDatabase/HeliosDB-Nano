//! MySQL SSL/TLS integration tests.
//!
//! Speaks the MySQL wire protocol at the raw-packet level (no `mysql` crate
//! dependency) rather than through a client library, mirroring how
//! `tests/postgres_ssl_tests.rs` drives its server with raw `TcpStream`
//! reads/writes. Nano's MySQL listener is trust-authenticated, so a
//! zero-length auth response is accepted for any username — these tests
//! exercise the TLS negotiation and PQ hybrid key-exchange path, not
//! credential verification.

use heliosdb_nano::protocol::mysql::{MysqlServer, MysqlServerConfig, MysqlSslConfig};
use heliosdb_nano::protocol::tls_provider::pq_capable_provider;
use heliosdb_nano::{EmbeddedDatabase, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_SSL: u32 = 0x0000_0800;

/// Probe-derive a free loopback port (same rationale as
/// `postgres_ssl_tests::free_loopback_port`: hardcoded ports collide with
/// long-running containers on shared hosts).
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral probe port")
        .local_addr()
        .expect("probe local_addr")
        .port()
}

async fn read_packet<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], 0]) as usize;
    let seq = hdr[3];
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((seq, payload))
}

async fn write_packet<S: AsyncWrite + Unpin>(stream: &mut S, seq: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.push((len & 0xFF) as u8);
    buf.push(((len >> 8) & 0xFF) as u8);
    buf.push(((len >> 16) & 0xFF) as u8);
    buf.push(seq);
    buf.extend_from_slice(payload);
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

/// Build a truncated `SSLRequest` payload — capability flags (with
/// `CLIENT_SSL` set) + max packet size + charset + 23 reserved bytes, no
/// username/auth yet, matching what a real MySQL client sends before
/// upgrading to TLS.
fn build_ssl_request(capabilities: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&capabilities.to_le_bytes());
    buf.extend_from_slice(&16_777_216u32.to_le_bytes());
    buf.push(45); // utf8mb4_general_ci
    buf.extend_from_slice(&[0u8; 23]);
    buf
}

/// Build a full `HandshakeResponse41` payload for trust auth: capability
/// flags, max packet size, charset, 23 reserved bytes, null-terminated
/// username, then a single zero byte (CLIENT_SECURE_CONNECTION empty
/// auth-response). No database, no plugin name, no connect attrs — this
/// client doesn't set those capability bits.
fn build_handshake_response(capabilities: u32, username: &str) -> Vec<u8> {
    let mut buf = build_ssl_request(capabilities);
    buf.extend_from_slice(username.as_bytes());
    buf.push(0); // null terminator
    buf.push(0); // auth-response length = 0
    buf
}

/// Accepts any server certificate — this suite tests TLS negotiation and
/// key-exchange group selection, not certificate-chain validation, against
/// the self-signed test cert `CertificateManager::setup_test_certs()` writes.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn client_config(post_quantum: bool) -> Arc<ClientConfig> {
    let provider = pq_capable_provider(post_quantum);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 only (PQ hybrid key exchange is TLS 1.3-only)")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Arc::new(config)
}

/// Generate a self-signed test cert/key pair into a per-test temp
/// directory — NOT the shared `certs/server.{crt,key}` path
/// `CertificateManager::setup_test_certs()` writes: several of these tests
/// build a real `rustls::ServerConfig` and run in parallel within the same
/// test binary, and two of them regenerating the SAME shared file pair
/// raced and intermittently produced a mismatched cert/key
/// ("keys may not be consistent: KeyMismatch") — see the identical fix in
/// `tests/postgres_ssl_tests.rs`.
fn setup_test_certs() -> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)> {
    use heliosdb_nano::protocol::postgres::CertificateManager;
    let temp_dir = tempfile::TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("temp dir: {}", e)))?;
    let cert_path = temp_dir.path().join("server.crt");
    let key_path = temp_dir.path().join("server.key");
    // `generate_test_cert()`'s hardcoded fallback PEM template doesn't
    // actually parse — use the real `openssl`-backed generator instead
    // (same one `setup_test_certs()` on `CertificateManager` uses).
    CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;
    Ok((temp_dir, cert_path, key_path))
}

async fn start_mysql_server(ssl: Option<MysqlSslConfig>) -> Result<SocketAddr> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = format!("127.0.0.1:{}", free_loopback_port())
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;
    let mut config = MysqlServerConfig::with_address(addr);
    if let Some(ssl) = ssl {
        config = config.with_ssl(ssl);
    }
    let server = MysqlServer::new(config, db)?;
    let server_addr = server.config().address;
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(server_addr)
}

/// Non-TLS client: read the greeting, send a full `HandshakeResponse41`
/// directly (no `CLIENT_SSL`), and expect an OK packet back.
async fn plaintext_login(addr: SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "test_user");
    write_packet(&mut stream, 1, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    let (_seq, ok_payload) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(
        ok_payload.first().copied(),
        Some(0x00),
        "expected an OK packet (0x00), got {:?}",
        ok_payload
    );
    Ok(())
}

/// TLS client: read the greeting, send a truncated SSLRequest, complete a
/// real rustls handshake, then send the full `HandshakeResponse41` over the
/// encrypted stream and expect an OK packet.
async fn tls_login(addr: SocketAddr, cfg: Arc<ClientConfig>) -> Result<rustls::NamedGroup> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let ssl_request = build_ssl_request(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL);
    write_packet(&mut stream, 1, &ssl_request)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("SSLRequest write failed: {}", e)))?;

    let connector = TlsConnector::from(cfg);
    let server_name = ServerName::try_from("localhost")
        .map_err(|e| heliosdb_nano::Error::network(format!("invalid server name: {}", e)))?
        .to_owned();
    let mut tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("TLS handshake failed: {}", e)))?;

    let group = tls_stream
        .get_ref()
        .1
        .negotiated_key_exchange_group()
        .ok_or_else(|| heliosdb_nano::Error::network("no key-exchange group negotiated".to_string()))?
        .name();

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL, "test_user");
    write_packet(&mut tls_stream, 2, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    let (_seq, ok_payload) = read_packet(&mut tls_stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(
        ok_payload.first().copied(),
        Some(0x00),
        "expected an OK packet (0x00), got {:?}",
        ok_payload
    );

    Ok(group)
}

// ---------------------------------------------------------------------
// Item 1: per-connection negotiated-KX-group visibility
// ---------------------------------------------------------------------

/// Decode a length-encoded integer at `buf[*pos]`, advancing `*pos` past it.
/// Only the single-byte and 0xFC (2-byte) forms are needed here — MySQL
/// result-set column counts and short string lengths never exceed that.
fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xfc => {
            let v = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]) as u64;
            *pos += 2;
            v
        }
        _ => first as u64,
    }
}

/// Decode a length-encoded string at `buf[*pos]`, advancing `*pos` past it.
fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_int(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).into_owned();
    *pos += len;
    s
}

/// Send `SELECT @@ssl_kx_group` over an already-authenticated connection
/// (plain or TLS) and return the single string value in the result set.
/// Speaks just enough of the MySQL text result-set protocol (column count,
/// column defs, EOF, one row, EOF) — this listener never sets
/// `CLIENT_DEPRECATE_EOF` in these tests' handshake, so EOF markers are
/// always present.
async fn query_ssl_kx_group<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<String> {
    let sql = "SELECT @@ssl_kx_group";
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03); // COM_QUERY
    payload.extend_from_slice(sql.as_bytes());
    write_packet(stream, 0, &payload)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("COM_QUERY write failed: {}", e)))?;

    // Column count packet.
    let (_seq, col_count_payload) = read_packet(stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("column-count read failed: {}", e)))?;
    let mut pos = 0usize;
    let ncols = read_lenenc_int(&col_count_payload, &mut pos);
    assert_eq!(ncols, 1, "SELECT @@ssl_kx_group must return exactly one column");

    // One column-definition packet per column.
    for _ in 0..ncols {
        read_packet(stream)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("column-def read failed: {}", e)))?;
    }

    // EOF after column defs.
    read_packet(stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("post-column-def EOF read failed: {}", e)))?;

    // One row packet: a single length-encoded string.
    let (_seq, row_payload) = read_packet(stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("row read failed: {}", e)))?;
    let mut pos = 0usize;
    let value = read_lenenc_str(&row_payload, &mut pos);

    // Closing EOF.
    read_packet(stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("closing EOF read failed: {}", e)))?;

    Ok(value)
}

/// TLS client that logs in AND reports the SQL-visible `@@ssl_kx_group`
/// value, so a test can assert it agrees with what the client itself
/// negotiated (`tls_stream`'s own `negotiated_key_exchange_group()`).
async fn tls_login_and_query_kx_group(
    addr: SocketAddr,
    cfg: Arc<ClientConfig>,
) -> Result<(rustls::NamedGroup, String)> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let ssl_request = build_ssl_request(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL);
    write_packet(&mut stream, 1, &ssl_request)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("SSLRequest write failed: {}", e)))?;

    let connector = TlsConnector::from(cfg);
    let server_name = ServerName::try_from("localhost")
        .map_err(|e| heliosdb_nano::Error::network(format!("invalid server name: {}", e)))?
        .to_owned();
    let mut tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("TLS handshake failed: {}", e)))?;

    let group = tls_stream
        .get_ref()
        .1
        .negotiated_key_exchange_group()
        .ok_or_else(|| heliosdb_nano::Error::network("no key-exchange group negotiated".to_string()))?
        .name();

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL, "test_user");
    write_packet(&mut tls_stream, 2, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    let (_seq, ok_payload) = read_packet(&mut tls_stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(
        ok_payload.first().copied(),
        Some(0x00),
        "expected an OK packet (0x00), got {:?}",
        ok_payload
    );

    let reported = query_ssl_kx_group(&mut tls_stream).await?;
    Ok((group, reported))
}

#[tokio::test]
async fn pq_client_ssl_kx_group_matches_negotiated_group() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let (group, reported) = tokio::time::timeout(
        Duration::from_secs(10),
        tls_login_and_query_kx_group(addr, client_config(true)),
    )
    .await
    .expect("TLS login + query timed out")?;
    assert_eq!(group, rustls::NamedGroup::X25519MLKEM768);
    assert_eq!(
        reported, "X25519MLKEM768",
        "@@ssl_kx_group must report the group the client actually negotiated"
    );
    Ok(())
}

#[tokio::test]
async fn classical_client_ssl_kx_group_matches_negotiated_group() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let (group, reported) = tokio::time::timeout(
        Duration::from_secs(10),
        tls_login_and_query_kx_group(addr, client_config(false)),
    )
    .await
    .expect("TLS login + query timed out")?;
    assert_ne!(group, rustls::NamedGroup::X25519MLKEM768);
    assert_eq!(
        reported,
        format!("{:?}", group),
        "@@ssl_kx_group must report the group the client actually negotiated"
    );
    assert_ne!(reported, "X25519MLKEM768");
    Ok(())
}

#[tokio::test]
async fn plaintext_connection_reports_empty_ssl_kx_group() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path);
    let addr = start_mysql_server(Some(ssl)).await?;

    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;
    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "test_user");
    write_packet(&mut stream, 1, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;
    let (_seq, ok_payload) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(ok_payload.first().copied(), Some(0x00));

    let reported = tokio::time::timeout(Duration::from_secs(10), query_ssl_kx_group(&mut stream))
        .await
        .expect("query timed out")?;
    assert_eq!(reported, "", "a plaintext connection must report an empty ssl_kx_group");
    Ok(())
}

// ---------------------------------------------------------------------
// Item 2: mutual TLS (client certificate verification)
// ---------------------------------------------------------------------

/// Generate a CA (self-signed) and a leaf certificate signed by it, using
/// the `openssl` CLI — the same tool `CertificateManager::generate_self_signed`
/// shells out to. Returns `(ca_cert, ca_key, leaf_cert, leaf_key)` paths, all
/// inside `dir`.
fn generate_ca_and_signed_cert(
    dir: &std::path::Path,
    leaf_cn: &str,
) -> Result<(
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
)> {
    let run = |args: &[&str]| -> Result<()> {
        let output = std::process::Command::new("openssl")
            .args(args)
            .output()
            .map_err(|e| heliosdb_nano::Error::io(format!("failed to execute openssl: {}", e)))?;
        if !output.status.success() {
            return Err(heliosdb_nano::Error::io(format!(
                "openssl {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    };

    // Namespaced by `leaf_cn` — two calls into the SAME temp dir (as the
    // wrong-CA test does, on purpose) must each get their own CA key/cert
    // files, or the second call's CA silently overwrites the first's.
    let ca_key = dir.join(format!("{leaf_cn}-ca.key"));
    let ca_cert = dir.join(format!("{leaf_cn}-ca.crt"));
    let leaf_key = dir.join(format!("{leaf_cn}.key"));
    let leaf_csr = dir.join(format!("{leaf_cn}.csr"));
    let leaf_cert = dir.join(format!("{leaf_cn}.crt"));

    // Self-signed CA.
    run(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        ca_key.to_str().expect("path"),
        "-out",
        ca_cert.to_str().expect("path"),
        "-days",
        "365",
        "-subj",
        "/CN=Test CA",
    ])?;

    // Leaf key + CSR.
    run(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        leaf_key.to_str().expect("path"),
        "-out",
        leaf_csr.to_str().expect("path"),
        "-subj",
        &format!("/CN={leaf_cn}"),
    ])?;

    // Sign the leaf with the CA. `openssl x509 -req` without `-extfile` emits
    // an X.509v1 certificate on some OpenSSL builds (no extensions block) —
    // rustls's webpki verifier rejects v1 certs with `UnsupportedCertVersion`.
    // Force v3 explicitly so this is stable across OpenSSL versions/distros.
    let extfile = dir.join(format!("{leaf_cn}.ext"));
    std::fs::write(
        &extfile,
        "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth\n",
    )
    .map_err(|e| heliosdb_nano::Error::io(format!("failed to write {extfile:?}: {e}")))?;
    run(&[
        "x509",
        "-req",
        "-in",
        leaf_csr.to_str().expect("path"),
        "-CA",
        ca_cert.to_str().expect("path"),
        "-CAkey",
        ca_key.to_str().expect("path"),
        "-CAcreateserial",
        "-out",
        leaf_cert.to_str().expect("path"),
        "-days",
        "365",
        "-extfile",
        extfile.to_str().expect("path"),
    ])?;

    Ok((ca_cert, ca_key, leaf_cert, leaf_key))
}

/// TLS client that, in addition to verifying the server cert, presents its
/// OWN certificate/key for mutual TLS.
async fn mtls_login(addr: SocketAddr, client_cert: &std::path::Path, client_key: &std::path::Path) -> Result<()> {
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use std::fs::File;
    use std::io::BufReader;

    let cert_chain: Vec<_> =
        certs(&mut BufReader::new(File::open(client_cert).map_err(|e| {
            heliosdb_nano::Error::io(format!("open client cert: {}", e))
        })?))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| heliosdb_nano::Error::io(format!("parse client cert: {}", e)))?;
    let mut keys: Vec<_> = pkcs8_private_keys(&mut BufReader::new(
        File::open(client_key).map_err(|e| heliosdb_nano::Error::io(format!("open client key: {}", e)))?,
    ))
    .collect::<std::result::Result<Vec<_>, _>>()
    .map_err(|e| heliosdb_nano::Error::io(format!("parse client key: {}", e)))?;
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0));

    let provider = pq_capable_provider(true);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| heliosdb_nano::Error::network(format!("client auth cert config: {}", e)))?;

    let group = tls_login(addr, Arc::new(config)).await?;
    let _ = group;
    Ok(())
}

#[tokio::test]
async fn mtls_client_with_ca_signed_cert_succeeds() -> Result<()> {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let (ca_cert, _ca_key, client_cert, client_key) = generate_ca_and_signed_cert(temp_dir.path(), "test-client")?;

    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_client_cert_verification(&ca_cert);
    let addr = start_mysql_server(Some(ssl)).await?;

    tokio::time::timeout(Duration::from_secs(10), mtls_login(addr, &client_cert, &client_key))
        .await
        .expect("mTLS login timed out")
}

#[tokio::test]
async fn mtls_client_with_no_cert_is_rejected() -> Result<()> {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let (ca_cert, _ca_key, _client_cert, _client_key) = generate_ca_and_signed_cert(temp_dir.path(), "unused")?;

    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_client_cert_verification(&ca_cert);
    let addr = start_mysql_server(Some(ssl)).await?;

    // `client_config(true)` presents no client certificate at all
    // (`.with_no_client_auth()`); the mTLS-required server must refuse the
    // handshake.
    let result = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true))).await;
    match result {
        Ok(Ok(_)) => panic!("a client presenting no certificate must not complete an mTLS handshake"),
        Ok(Err(_)) | Err(_) => {} // handshake failure or timeout: both are an acceptable rejection
    }
    Ok(())
}

#[tokio::test]
async fn mtls_client_with_wrong_ca_cert_is_rejected() -> Result<()> {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let (ca_cert, _ca_key, _client_cert, _client_key) = generate_ca_and_signed_cert(temp_dir.path(), "server-ca")?;

    // A second, unrelated CA signs the "client" certificate — the server
    // only trusts the first CA.
    let (_other_ca_cert, _other_ca_key, wrong_client_cert, wrong_client_key) =
        generate_ca_and_signed_cert(temp_dir.path(), "wrong-ca-client")?;

    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_client_cert_verification(&ca_cert);
    let addr = start_mysql_server(Some(ssl)).await?;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        mtls_login(addr, &wrong_client_cert, &wrong_client_key),
    )
    .await;
    match result {
        Ok(Ok(())) => panic!("a certificate signed by the wrong CA must not be accepted"),
        Ok(Err(_)) | Err(_) => {} // handshake failure or timeout: both are an acceptable rejection
    }
    Ok(())
}

#[tokio::test]
async fn plain_client_connects_when_tls_not_configured() -> Result<()> {
    let addr = start_mysql_server(None).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}

#[tokio::test]
async fn plain_client_still_connects_when_tls_enabled_but_not_required() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}

#[tokio::test]
async fn pq_only_client_negotiates_hybrid_group_against_pq_server() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    assert_eq!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "a PQ-capable client against a PQ-enabled MySQL server must negotiate the hybrid group"
    );
    Ok(())
}

#[tokio::test]
async fn classical_only_client_still_succeeds_against_pq_mysql_server() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(false)))
        .await
        .expect("TLS login timed out")?;
    assert_ne!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "a classical-only client cannot have negotiated the PQ hybrid group"
    );
    Ok(())
}

#[tokio::test]
async fn pq_disabled_mysql_server_never_offers_hybrid_group() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(false);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    assert_ne!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "mysql_tls_post_quantum = false must not offer the PQ hybrid group"
    );
    Ok(())
}

// ---------------------------------------------------------------------
// require_tls
// ---------------------------------------------------------------------
//
// `MysqlSslConfig::with_require_tls(true)` must actually reject a
// plaintext client rather than being a no-op (review finding #2).

/// A plaintext handshake against a `require_tls = true` server must be
/// rejected: the connection either closes without an OK packet, or
/// delivers an ERR packet — never the OK a successful login would produce.
async fn expect_plaintext_rejected(addr: SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "test_user");
    write_packet(&mut stream, 1, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    match read_packet(&mut stream).await {
        Ok((_seq, payload)) => {
            assert_ne!(
                payload.first().copied(),
                Some(0x00),
                "require_tls=true must not let a plaintext client through with an OK packet"
            );
            // Non-OK (typically an ERR 0xFF packet) is the rejection.
        }
        Err(_) => {
            // The server closed the connection outright — also an
            // acceptable rejection.
        }
    }
    Ok(())
}

#[tokio::test]
async fn require_tls_rejects_plaintext_client() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), expect_plaintext_rejected(addr))
        .await
        .expect("rejection check timed out")
}

#[tokio::test]
async fn require_tls_still_accepts_tls_client() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    Ok(())
}

#[tokio::test]
async fn require_tls_false_plaintext_client_still_succeeds() -> Result<()> {
    // No-regression check: `require_tls = false` (the default) must keep
    // allowing plaintext clients even when TLS is enabled/offered.
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(false);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}
