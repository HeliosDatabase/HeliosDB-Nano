//! Password logins on the MySQL listener.
//!
//! The listener used to accept every login. With a method that asks for a
//! password it now authenticates with `caching_sha2_password` against the
//! same accounts and roles as the PostgreSQL wire: full authentication over a
//! secure transport (TLS, the Unix socket), full authentication over plain TCP
//! through the RSA public-key exchange, and the fast path after one full
//! authentication. The client in `mysql_wire_client` computes the plugin's
//! public formulas itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

mod mysql_wire_client;

use heliosdb_nano::protocol::mysql::{MysqlServer, MysqlServerConfig, MysqlSslConfig};
use heliosdb_nano::protocol::postgres::CertificateManager;
use heliosdb_nano::protocol::tls_provider::pq_capable_provider;
use mysql_wire_client::*;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

const ADMIN: (&str, &str) = ("admin", "admin-secret");

fn refusal(login: &Login) -> (u16, String, String) {
    match login {
        Login::Refused { code, state, message } => (*code, state.clone(), message.clone()),
        other => panic!("expected a refused login, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The plugin, per transport
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_password_logs_in_on_a_secure_transport_and_the_session_is_that_user() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert_eq!(c.greeting.plugin, CACHING_SHA2, "the plugin the listener names");
    assert_eq!(c.greeting.nonce.len(), 20);

    let login = c.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::Cleartext).await;
    assert_eq!(login, Login::Accepted { fast: false }, "the first login is a full authentication");
    assert_eq!(c.scalar("SELECT current_user").await, "admin");
    assert_eq!(auth.full_verifications(), 1);
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_user_are_refused_alike_after_the_same_work() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);

    let mut seen = Vec::new();
    for (user, password) in [(ADMIN.0, "not-the-password"), ("nobody", "not-the-password")] {
        let before = auth.full_verifications();
        let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
        let scramble = sha2_scramble(password, &c.nonce);
        c.send_handshake_response(CAPS, user, &scramble, None, Some(CACHING_SHA2))
            .await;
        // The same packets for both: "send the password", then the refusal.
        let more = c.read_packet().await;
        assert_eq!(more, [0x01, 0x04], "{user}");
        let mut clear = password.as_bytes().to_vec();
        clear.push(0);
        c.reply(&clear).await;
        let Answer::Err { code, state, message } = decode_err(&c.read_packet().await) else {
            unreachable!()
        };
        assert_eq!((code, state.as_str()), (1045, "28000"), "{user}");
        // One key derivation each, whether the account exists or not.
        assert_eq!(auth.full_verifications() - before, 1, "{user}");
        assert!(c.try_read_packet().await.is_none(), "the connection ends with the refusal");
        seen.push(message.replace(user, "<user>"));
    }
    assert_eq!(seen[0], seen[1], "one message for both");
    assert_eq!(seen[0], "Access denied for user '<user>'@'%' (using password: YES)");
    assert_eq!(auth.failed_logins(), 2);
}

#[tokio::test]
async fn plain_tcp_logs_in_through_the_rsa_public_key_exchange() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);

    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    let login = c.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::PublicKey).await;
    assert_eq!(login, Login::Accepted { fast: false });
    assert_eq!(c.scalar("SELECT 40 + 2").await, "42");

    // A password longer than the scramble it is XOR-ed with.
    let long = "a-password-that-is-longer-than-the-twenty-bytes-of-the-scramble";
    db.execute(&format!("CREATE ROLE longpw LOGIN PASSWORD '{long}'")).unwrap();
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert!(c.login_sha2(CAPS, "longpw", long, FullAuth::PublicKey).await.accepted());

    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    let refused = c.login_sha2(CAPS, ADMIN.0, "wrong", FullAuth::PublicKey).await;
    assert_eq!(refusal(&refused).0, 1045);
}

#[tokio::test]
async fn a_password_sent_as_it_is_over_plain_tcp_is_refused_and_never_checked() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    // The right password, in the clear, on a connection anyone can read.
    let login = c.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::Cleartext).await;
    let (code, state, message) = refusal(&login);
    assert_eq!((code, state.as_str()), (1045, "28000"));
    assert!(message.contains("TLS"), "{message}");
    assert!(message.contains("Unix socket"), "{message}");
    assert_eq!(auth.full_verifications(), 0, "the password was not looked at");
}

#[tokio::test]
async fn the_fast_path_follows_a_full_authentication_and_ends_with_the_password() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    db.execute("CREATE ROLE app LOGIN PASSWORD 'one'").unwrap();

    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(
        c.login_sha2(CAPS, "app", "one", FullAuth::PublicKey).await,
        Login::Accepted { fast: false }
    );
    // The second login is accepted from the scramble alone, on plain TCP too.
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(
        c.login_sha2(CAPS, "app", "one", FullAuth::PublicKey).await,
        Login::Accepted { fast: true }
    );
    assert_eq!(auth.fast_successes(), 1);
    assert_eq!(auth.full_verifications(), 1);
    // A wrong scramble is not taken from the cache: it falls to a full check.
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(refusal(&c.login_sha2(CAPS, "app", "two", FullAuth::PublicKey).await).0, 1045);

    // The password changes: the old one stops working at once, the new one
    // needs a full authentication again.
    db.execute("ALTER ROLE app PASSWORD 'two'").unwrap();
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(refusal(&c.login_sha2(CAPS, "app", "one", FullAuth::PublicKey).await).0, 1045);
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(
        c.login_sha2(CAPS, "app", "two", FullAuth::PublicKey).await,
        Login::Accepted { fast: false }
    );

    // The role is dropped: nothing cached logs it in.
    db.execute("DROP ROLE app").unwrap();
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert_eq!(refusal(&c.login_sha2(CAPS, "app", "two", FullAuth::PublicKey).await).0, 1045);
}

#[tokio::test]
async fn a_client_that_insists_on_mysql_native_password_is_switched() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let mut c = open(&db, Some(auth), true).await;
    let greeting_nonce = c.nonce.clone();
    // What a native-password client sends first: 20 bytes of its own scramble.
    c.send_handshake_response(CAPS, ADMIN.0, &[0x5a; 20], None, Some(NATIVE))
        .await;
    assert_eq!(c.read_auth_switch().await, CACHING_SHA2);
    assert_eq!(c.nonce.len(), 20);
    assert_ne!(c.nonce, greeting_nonce, "the switch carries a scramble of its own");
    let scramble = sha2_scramble(ADMIN.1, &c.nonce);
    c.reply(&scramble).await;
    assert!(c.finish_sha2(ADMIN.1, FullAuth::Cleartext).await.accepted());
    assert_eq!(c.scalar("SELECT current_user").await, "admin");
}

#[tokio::test]
async fn mysql_clear_password_is_taken_on_a_secure_transport_only() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let mut clear = ADMIN.1.as_bytes().to_vec();
    clear.push(0);

    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    c.send_handshake_response(CAPS, ADMIN.0, &clear, None, Some(CLEAR)).await;
    assert_eq!(c.read_packet().await[0], 0x00, "accepted on a secure transport");

    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    c.send_handshake_response(CAPS, ADMIN.0, b"wrong\0", None, Some(CLEAR)).await;
    assert_eq!(decode_err(&c.read_packet().await).err_code(), Some(1045));

    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    c.send_handshake_response(CAPS, ADMIN.0, &clear, None, Some(CLEAR)).await;
    let refused = decode_err(&c.read_packet().await);
    assert_eq!(refused.err_code(), Some(1045));
    assert!(refused.message().contains("TLS"), "{}", refused.message());
}

#[tokio::test]
async fn an_empty_password_never_logs_in() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    // A role that may log in and has no password at all.
    db.execute("CREATE ROLE nopw LOGIN").unwrap();
    for user in [ADMIN.0, "nopw", "nobody"] {
        for secure in [true, false] {
            let mut c = open(&db, Some(Arc::clone(&auth)), secure).await;
            let login = c.login_sha2(CAPS, user, "", FullAuth::Cleartext).await;
            let (code, _, message) = refusal(&login);
            assert_eq!(code, 1045, "{user}");
            assert!(message.ends_with("(using password: NO)"), "{message}");
        }
    }
}

// ---------------------------------------------------------------------------
// Roles and account statements
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_role_with_login_and_a_password_logs_in_and_others_do_not() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    db.execute("CREATE ROLE reporter LOGIN PASSWORD 'r-pass'").unwrap();
    db.execute("CREATE ROLE grouponly PASSWORD 'g-pass'").unwrap();

    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(c.login_sha2(CAPS, "reporter", "r-pass", FullAuth::Cleartext).await.accepted());
    assert_eq!(c.scalar("SELECT current_user").await, "reporter");
    assert_eq!(c.scalar("SELECT session_user").await, "reporter");

    // A role without LOGIN is refused like any unknown name.
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    let (code, _, message) = refusal(&c.login_sha2(CAPS, "grouponly", "g-pass", FullAuth::Cleartext).await);
    assert_eq!(code, 1045);
    assert_eq!(message, "Access denied for user 'grouponly'@'%' (using password: YES)");
}

#[tokio::test]
async fn create_alter_and_drop_user_act_on_the_roles() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let mut admin = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(admin.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::Cleartext).await.accepted());

    admin.ok("CREATE USER 'wp'@'%' IDENTIFIED BY 'wp-pass'").await;
    let (rows, _) = db
        .query_with_columns("SELECT rolname, rolcanlogin FROM pg_roles WHERE rolname = 'wp'")
        .unwrap();
    assert_eq!(rows.len(), 1, "CREATE USER made a role");

    let mut wp = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(wp.login_sha2(CAPS, "wp", "wp-pass", FullAuth::Cleartext).await.accepted());
    assert_eq!(wp.scalar("SELECT current_user").await, "wp");

    // The same name again is refused; IF NOT EXISTS leaves the account as it is.
    assert_eq!(admin.err("CREATE USER 'wp'@'localhost' IDENTIFIED BY 'x'").await.0, 1396);
    admin.ok("CREATE USER IF NOT EXISTS 'wp'@'%' IDENTIFIED BY 'ignored'").await;
    let mut again = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(again.login_sha2(CAPS, "wp", "wp-pass", FullAuth::Cleartext).await.accepted());

    // A host this server cannot bind an account to is refused, not dropped.
    let (code, message) = admin.err("CREATE USER 'app'@'10.0.0.%' IDENTIFIED BY 'p'").await;
    assert_eq!(code, 1396);
    assert!(message.contains("'%' or 'localhost'"), "{message}");
    assert_eq!(admin.err("CREATE USER 'app'@'%' IDENTIFIED BY 'p' REQUIRE SSL").await.0, 1396);
    let (rows, _) = db
        .query_with_columns("SELECT rolname FROM pg_roles WHERE rolname = 'app'")
        .unwrap();
    assert!(rows.is_empty(), "a refused CREATE USER made nothing");

    // ALTER USER and SET PASSWORD change the password both wires check.
    admin.ok("ALTER USER 'wp'@'%' IDENTIFIED BY 'second'").await;
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert_eq!(refusal(&c.login_sha2(CAPS, "wp", "wp-pass", FullAuth::Cleartext).await).0, 1045);
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(c.login_sha2(CAPS, "wp", "second", FullAuth::Cleartext).await.accepted());

    admin.ok("SET PASSWORD FOR 'wp'@'%' = 'third'").await;
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(c.login_sha2(CAPS, "wp", "third", FullAuth::Cleartext).await.accepted());
    // Its own password, by the session that owns the account.
    c.ok("SET PASSWORD = 'fourth'").await;
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(c.login_sha2(CAPS, "wp", "fourth", FullAuth::Cleartext).await.accepted());

    assert_eq!(admin.err("ALTER USER 'ghost'@'%' IDENTIFIED BY 'x'").await.0, 1396);
    admin.ok("ALTER USER IF EXISTS 'ghost'@'%' IDENTIFIED BY 'x'").await;

    admin.ok("DROP USER 'wp'@'%'").await;
    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert_eq!(refusal(&c.login_sha2(CAPS, "wp", "fourth", FullAuth::Cleartext).await).0, 1045);
    assert_eq!(admin.err("DROP USER 'wp'@'%'").await.0, 1396);
    admin.ok("DROP USER IF EXISTS 'wp'@'%'").await;
}

// ---------------------------------------------------------------------------
// COM_CHANGE_USER
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_user_authenticates_again_and_starts_a_new_session() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    db.execute("CREATE ROLE app LOGIN PASSWORD 'app-pass'").unwrap();
    db.execute("CREATE TABLE cu (a INT PRIMARY KEY)").unwrap();

    let mut c = open(&db, Some(Arc::clone(&auth)), true).await;
    assert!(c.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::Cleartext).await.accepted());
    c.ok("SET @carried = 5").await;
    c.ok("SET autocommit = 0").await;
    c.ok("INSERT INTO cu VALUES (1)").await;

    assert!(c.change_user("app", "app-pass", FullAuth::Cleartext).await.accepted());
    assert_eq!(c.scalar("SELECT current_user").await, "app");
    assert_eq!(c.scalar("SELECT @carried").await, "NULL", "user variables do not survive");
    assert_eq!(c.scalar("SELECT @@autocommit").await, "1", "session variables do not survive");
    assert_eq!(c.scalar("SELECT COUNT(*) FROM cu").await, "0", "the open transaction was rolled back");

    // A wrong password ends the connection: it does not stay logged in as before.
    let refused = c.change_user(ADMIN.0, "wrong", FullAuth::Cleartext).await;
    assert_eq!(refusal(&refused).0, 1045);
    c.write_packet(0, &[COM_PING]).await;
    assert!(c.try_read_packet().await.is_none(), "the connection is closed");
}

#[tokio::test]
async fn change_user_on_plain_tcp_uses_the_public_key_exchange() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    db.execute("CREATE ROLE app LOGIN PASSWORD 'app-pass'").unwrap();
    let mut c = open(&db, Some(Arc::clone(&auth)), false).await;
    assert!(c.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::PublicKey).await.accepted());
    assert!(c.change_user("app", "app-pass", FullAuth::PublicKey).await.accepted());
    assert_eq!(c.scalar("SELECT current_user").await, "app");
}

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_trust_listener_greets_and_accepts_as_before() {
    let db = test_db();
    let mut c = open(&db, None, false).await;
    assert_eq!(c.greeting.plugin, NATIVE, "the plugin a trust listener has always named");
    assert_eq!(c.greeting.raw.len(), c.greeting.server_version.len() + 1 + 1 + 4 + 9 + 2 + 1 + 2 + 2 + 1 + 10 + 13 + NATIVE.len() + 1);
    // Any name, no password, no plugin negotiation: the pre-authentication client.
    c.send_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "anyone", &[], None, None)
        .await;
    assert_eq!(c.read_packet().await[0], 0x00);
    assert_eq!(c.scalar("SELECT current_user").await, "anyone");
}

// ---------------------------------------------------------------------------
// The TCP listener: plain and TLS
// ---------------------------------------------------------------------------

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
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

async fn connect(addr: SocketAddr) -> TcpStream {
    for _ in 0..100 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the listener on {addr} did not come up");
}

#[tokio::test]
async fn the_tcp_listener_authenticates_over_plain_tcp_and_over_tls() {
    let db = test_db();
    let auth = scram_auth(&[ADMIN]);
    let dir = tempfile::TempDir::new().unwrap();
    let (cert, key) = (dir.path().join("server.crt"), dir.path().join("server.key"));
    CertificateManager::generate_self_signed(&cert, &key, "localhost").unwrap();

    let addr: SocketAddr = format!("127.0.0.1:{}", free_loopback_port()).parse().unwrap();
    let config = MysqlServerConfig::with_address(addr)
        .with_auth(Some(Arc::clone(&auth)))
        .with_ssl(MysqlSslConfig::new(&cert, &key));
    let server = MysqlServer::new(config, Arc::clone(&db)).unwrap();
    tokio::spawn(async move {
        let _ = server.serve().await;
    });

    // Plain TCP: the public-key exchange, and the connection id is the
    // session's id.
    let mut plain = Client::greet(connect(addr).await).await;
    assert_eq!(plain.greeting.plugin, CACHING_SHA2);
    let announced = plain.greeting.connection_id;
    assert!(plain.login_sha2(CAPS, ADMIN.0, ADMIN.1, FullAuth::PublicKey).await.accepted());
    assert_eq!(plain.scalar("SELECT CONNECTION_ID()").await, announced.to_string());

    // Plain TCP, the password as it is: refused, and not because it was wrong.
    let mut plain = Client::greet(connect(addr).await).await;
    // A fresh account so the fast path cannot answer first.
    db.execute("CREATE ROLE tcpuser LOGIN PASSWORD 'tcp-pass'").unwrap();
    let (code, _, message) = refusal(&plain.login_sha2(CAPS, "tcpuser", "tcp-pass", FullAuth::Cleartext).await);
    assert_eq!(code, 1045);
    assert!(message.contains("TLS"), "{message}");

    // TLS: the password travels as it is inside the encrypted stream.
    let mut tcp = Client::greet(connect(addr).await).await;
    assert_ne!(tcp.greeting.capabilities & CLIENT_SSL, 0, "the listener offers TLS");
    let mut ssl_request = Vec::new();
    ssl_request.extend_from_slice(&(CAPS | CLIENT_SSL).to_le_bytes());
    ssl_request.extend_from_slice(&(1u32 << 24).to_le_bytes());
    ssl_request.push(45);
    ssl_request.extend_from_slice(&[0u8; 23]);
    tcp.reply(&ssl_request).await;
    let tls_config = ClientConfig::builder_with_provider(pq_capable_provider(false))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let tls_stream = connector
        .connect(ServerName::try_from("localhost").unwrap().to_owned(), tcp.stream)
        .await
        .unwrap();
    let mut tls = Client {
        stream: tls_stream,
        greeting: tcp.greeting.clone(),
        caps: CAPS | CLIENT_SSL,
        last_seq: 1,
        nonce: tcp.nonce.clone(),
    };
    let login = tls
        .login_sha2(CAPS | CLIENT_SSL, "tcpuser", "tcp-pass", FullAuth::Cleartext)
        .await;
    assert_eq!(login, Login::Accepted { fast: false });
    assert_eq!(tls.scalar("SELECT current_user").await, "tcpuser");

    // Wrong password over TLS.
    drop(tls);
}
