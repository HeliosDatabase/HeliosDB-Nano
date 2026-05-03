//! Bug 2 + same-host-only `trust` enforcement — v3.26.0.
//!
//! Bug 2 was filed because the SCRAM-SHA-256 client-first-message parser
//! at `src/protocol/postgres/handler.rs:768-777` split the message on
//! commas and indexed the GS2 channel-binding flag as the username.
//! libpq actually sends `n,,n=user,r=nonce` — the leading `n,,` is the
//! GS2 header (RFC 5802), and the username/nonce live AFTER it.
//!
//! Same-host-only `trust` enforcement: a non-loopback `--listen` paired
//! with `--auth trust` would silently accept any client; v3.26.0 refuses
//! that combination at server-construction time.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use heliosdb_nano::EmbeddedDatabase;
use heliosdb_nano::protocol::postgres::{AuthManager, AuthMethod, PgServer, PgServerConfig};

// ---------- Bug 2: SCRAM client-first parser ---------------------------------

#[test]
fn scram_parser_handles_libpq_gs2_header() {
    // libpq sends: gs2-cbind-flag=n , authzid="" , n=user , r=nonce
    let msg = "n,,n=alice,r=client_nonce_abcdefgh";
    let (user, nonce) = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test(msg)
        .expect("must parse libpq client-first-message");
    assert_eq!(user, "alice");
    assert_eq!(nonce, "client_nonce_abcdefgh");
}

#[test]
fn scram_parser_handles_authzid() {
    // libpq with authzid: gs2-cbind-flag=n , authzid="a=other" , n=user , r=nonce
    let msg = "n,a=otheruser,n=alice,r=nonce123";
    let (user, nonce) = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test(msg)
        .expect("must parse client-first-message with authzid");
    assert_eq!(user, "alice");
    assert_eq!(nonce, "nonce123");
}

#[test]
fn scram_parser_handles_y_channel_binding_flag() {
    // gs2-cbind-flag = "y" means client does not support channel binding
    // even though server might.
    let msg = "y,,n=bob,r=xyz";
    let (user, nonce) = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test(msg)
        .expect("must parse y-flag client-first-message");
    assert_eq!(user, "bob");
    assert_eq!(nonce, "xyz");
}

#[test]
fn scram_parser_rejects_truncated_message() {
    // Missing the bare body after the GS2 header.
    let result = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test("n,,");
    assert!(result.is_err(), "missing bare body should be a parse error");
}

#[test]
fn scram_parser_rejects_missing_username() {
    let result = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test("n,,r=onlynonce");
    assert!(result.is_err(), "missing username (n=) should be a parse error");
}

#[test]
fn scram_parser_rejects_missing_nonce() {
    let result = heliosdb_nano::protocol::postgres::auth::parse_scram_client_first_for_test("n,,n=alice");
    assert!(result.is_err(), "missing nonce (r=) should be a parse error");
}

// ---------- same-host-only trust enforcement ---------------------------------

#[test]
fn trust_auth_on_loopback_is_allowed() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
    ));
    // default auth_method is Trust; this MUST succeed because 127.0.0.1 is loopback.
    let result = PgServer::new(config, db);
    assert!(
        result.is_ok(),
        "AuthMethod::Trust on 127.0.0.1 must be allowed; got {:?}",
        result.as_ref().err().map(|e| e.to_string()).unwrap_or_default()
    );
}

#[test]
fn trust_auth_on_ipv6_loopback_is_allowed() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        0,
    ));
    let result = PgServer::new(config, db);
    assert!(result.is_ok(), "trust on ::1 must be allowed; got {:?}", result.err().map(|e| e.to_string()).unwrap_or_default());
}

#[test]
fn trust_auth_on_unspecified_address_is_refused() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    // 0.0.0.0 means "all interfaces" — includes the public interface, so
    // trust is unsafe.
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        5432,
    ));
    let result = PgServer::new(config, db);
    assert!(
        result.is_err(),
        "AuthMethod::Trust on 0.0.0.0 must be refused at server construction"
    );
    // PgServer doesn't implement Debug; extract the error via match
    // rather than unwrap_err.
    let msg = match result {
        Ok(_) => unreachable!(),
        Err(e) => e.to_string().to_lowercase(),
    };
    assert!(
        msg.contains("trust") && (msg.contains("loopback") || msg.contains("127.0.0.1") || msg.contains("non-loopback")),
        "error must explain why trust is refused on non-loopback; got {msg}"
    );
}

#[test]
fn trust_auth_on_public_address_is_refused() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    // 192.0.2.1 is in TEST-NET-1 (RFC 5737) and not loopback.
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        5432,
    ));
    let result = PgServer::new(config, db);
    assert!(
        result.is_err(),
        "AuthMethod::Trust on a non-loopback IPv4 must be refused"
    );
}

#[test]
fn password_auth_on_public_address_is_allowed() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        5432,
    ))
    .with_auth_method(AuthMethod::CleartextPassword);
    let result = PgServer::new(config, db);
    assert!(
        result.is_ok(),
        "non-trust auth on public address must be allowed; got {:?}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

#[test]
fn scram_auth_on_public_address_is_allowed() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        5432,
    ))
    .with_auth_method(AuthMethod::ScramSha256);
    let result = PgServer::new(config, db);
    assert!(
        result.is_ok(),
        "SCRAM on public address must be allowed; got {:?}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

#[test]
fn with_auth_manager_also_enforces_trust_loopback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        5432,
    ));
    let auth_mgr = AuthManager::new(AuthMethod::Trust).with_default_users();
    let result = PgServer::with_auth_manager(config, db, auth_mgr);
    assert!(
        result.is_err(),
        "with_auth_manager must apply the same trust-loopback gate as new()"
    );
}
