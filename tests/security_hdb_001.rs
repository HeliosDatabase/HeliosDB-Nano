//! HDB-001 — account existence must not change the SCRAM wire exchange.
//!
//! WHAT WAS BROKEN. `handle_scram_authentication` resolved the account with
//! `get_credentials(username).ok_or_else(|| Error::authentication("User not found"))?`
//! BEFORE building the challenge. An unknown name therefore received
//! `AuthenticationSASL` and then a FATAL 08P01 "User not found", while a known
//! name with a wrong password received `AuthenticationSASL`,
//! `AuthenticationSASLContinue` and only then an error. Any unauthenticated peer
//! could enumerate accounts just by counting authentication messages — and the
//! error text named the condition outright.
//!
//! THE INVARIANT. Every failing password login must produce the SAME observable
//! exchange: the same authentication-message sequence, the same SQLSTATE, and
//! the same error fields — whether the name exists or not, and whatever the
//! client's proof was. An absent account is challenged with DETERMINISTIC
//! synthetic credentials (PostgreSQL's mock authentication), so the salt served
//! for a given name is stable across attempts and across restarts (a fresh
//! random salt per attempt would itself be an oracle) — and yet a proof that
//! verifies against those synthetic credentials still must not authenticate.
//!
//! THE CLEARTEXT ARM. `AuthenticationCleartextPassword` already produced the
//! same wire bytes for both cases, but not the same WORK: a known name paid a
//! full PBKDF2 inside `verify_password`, an unknown one a single SHA-256. It now
//! verifies against the same mock-authentication credential, and is asserted
//! here on the same observables.
//!
//! Everything below is driven through a real `PgConnectionHandler` over a real
//! TCP socket with an independently constructed SCRAM client, so what is
//! asserted is what actually goes on the wire.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use heliosdb_nano::protocol::postgres::auth::{scram_h, scram_hi, scram_hmac_sha256};
use heliosdb_nano::protocol::postgres::handler::PgConnectionHandler;
use heliosdb_nano::protocol::postgres::password_store::{
    InMemoryPasswordStore, PasswordStore, ScramCredentials, SharedPasswordStore,
};
use heliosdb_nano::protocol::postgres::{AuthManager, AuthMethod};
use heliosdb_nano::{EmbeddedDatabase, Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Everything an unauthenticated peer can observe from one login attempt.
#[derive(Debug)]
struct Exchange {
    auth_messages: Vec<u32>,
    salt: Vec<u8>,
    nonce: String,
    iterations: u32,
    error: BTreeMap<char, String>,
    ready: bool,
}

async fn message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).await.expect("backend header");
    let size = u32::from_be_bytes(header[1..].try_into().expect("length"));
    assert!((4..=65536).contains(&size), "invalid message size {size}");
    let mut payload = vec![0; (size - 4) as usize];
    stream.read_exact(&mut payload).await.expect("backend payload");
    (header[0], payload)
}

async fn send(stream: &mut TcpStream, tag: u8, payload: &[u8]) {
    let mut data = vec![tag];
    data.extend_from_slice(&((payload.len() + 4) as u32).to_be_bytes());
    data.extend_from_slice(payload);
    stream.write_all(&data).await.expect("frontend message");
}

fn error_fields(payload: &[u8]) -> BTreeMap<char, String> {
    payload
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| (char::from(f[0]), String::from_utf8_lossy(&f[1..]).into_owned()))
        .collect()
}

/// Run one full startup + SCRAM attempt against a real handler.
///
/// `mock_secret` makes the client derive its proof from the SYNTHETIC client
/// key for `user` instead of from a password, which is how the forged-proof
/// control below is built. `proof_override` replaces the `p=` field verbatim.
async fn exchange_with_mock_secret(
    auth: Arc<AuthManager>,
    user: &str,
    password: &str,
    mock_secret: Option<&[u8; 32]>,
    proof_override: Option<&str>,
) -> Exchange {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let mut client = TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("connect");
        let (socket, _) = listener.accept().await.expect("accept");
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("database"));
        let mut handler = PgConnectionHandler::new(socket, db, auth, None);
        let server = tokio::spawn(async move { Box::pin(handler.handle()).await });
        let params = format!("user\0{user}\0database\0heliosdb\0\0");
        let mut startup = ((params.len() + 8) as u32).to_be_bytes().to_vec();
        startup.extend_from_slice(&196608_u32.to_be_bytes());
        startup.extend_from_slice(params.as_bytes());
        client.write_all(&startup).await.expect("startup");
        let mut result = Exchange {
            auth_messages: Vec::new(),
            salt: Vec::new(),
            nonce: String::new(),
            iterations: 0,
            error: BTreeMap::new(),
            ready: false,
        };
        let first_bare = "n=,r=hdb001-client-nonce";
        loop {
            let (tag, payload) = message(&mut client).await;
            match tag {
                b'R' => {
                    let kind = u32::from_be_bytes(payload[..4].try_into().expect("auth kind"));
                    result.auth_messages.push(kind);
                    match kind {
                        10 => {
                            let first = format!("n,,{first_bare}");
                            let mut initial = b"SCRAM-SHA-256\0".to_vec();
                            initial.extend_from_slice(&(first.len() as u32).to_be_bytes());
                            initial.extend_from_slice(first.as_bytes());
                            send(&mut client, b'p', &initial).await;
                        }
                        11 => {
                            let challenge = std::str::from_utf8(&payload[4..]).expect("challenge UTF-8");
                            for part in challenge.split(',') {
                                if let Some(value) = part.strip_prefix("s=") {
                                    result.salt = BASE64_STANDARD.decode(value).expect("salt");
                                } else if let Some(value) = part.strip_prefix("r=") {
                                    result.nonce = value.to_owned();
                                } else if let Some(value) = part.strip_prefix("i=") {
                                    result.iterations = value.parse().expect("iterations");
                                }
                            }
                            assert!(!result.salt.is_empty());
                            assert!(result.iterations > 0);
                            let final_bare = format!("c=biws,r={}", result.nonce);
                            let auth_message = format!("{first_bare},{challenge},{final_bare}");
                            let client_key = if let Some(secret) = mock_secret {
                                synthetic_block(secret, b"client", user, 32)
                            } else {
                                let salted = scram_hi(password, &result.salt, result.iterations);
                                scram_hmac_sha256(&salted, b"Client Key")
                            };
                            let signature = scram_hmac_sha256(&scram_h(&client_key), auth_message.as_bytes());
                            let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
                            let proof_b64 = proof_override
                                .map(str::to_owned)
                                .unwrap_or_else(|| BASE64_STANDARD.encode(&proof));
                            let final_msg = format!("{final_bare},p={proof_b64}");
                            send(&mut client, b'p', final_msg.as_bytes()).await;
                        }
                        // AuthenticationCleartextPassword: the client answers
                        // with a NUL-terminated PasswordMessage. Shares this
                        // driver so the cleartext arm is asserted on exactly
                        // the same observables as the SCRAM arm.
                        3 => {
                            let mut body = password.as_bytes().to_vec();
                            body.push(0);
                            send(&mut client, b'p', &body).await;
                        }
                        0 | 12 => (),
                        other => panic!("unexpected authentication kind {other}"),
                    }
                }
                b'E' => {
                    // A FATAL startup error is the LAST message on the
                    // connection: PostgreSQL never follows it with a
                    // ReadyForQuery, and neither do we, so breaking here (and
                    // leaving `ready` false) is not a truncated read — waiting
                    // for a `Z` would hang until the socket closed.
                    result.error = error_fields(&payload);
                    break;
                }
                b'Z' => {
                    result.ready = true;
                    send(&mut client, b'X', &[]).await;
                    break;
                }
                b'S' | b'K' => (),
                other => panic!("unexpected backend message {other}"),
            }
        }
        drop(client);
        let server_result = server.await.expect("handler must not panic");
        assert_eq!(
            server_result.is_ok(),
            result.ready,
            "only an authenticated exchange should end successfully"
        );
        result
    })
    .await
    .expect("authentication exchange must finish")
}

async fn exchange(auth: Arc<AuthManager>, user: &str, password: &str) -> Exchange {
    exchange_with_mock_secret(auth, user, password, None, None).await
}

/// A custom backend that persists its own mock-authentication secret. It
/// implements exactly the `PasswordStore` surface that exists — the two new
/// methods are defaulted, so an embedder only overrides what it cares about.
struct PolicyStore {
    users: HashMap<String, ScramCredentials>,
    iterations: u32,
    secret: [u8; 32],
}

impl PasswordStore for PolicyStore {
    fn get_credentials(&self, username: &str) -> Option<ScramCredentials> {
        self.users.get(username).cloned()
    }

    fn default_scram_iterations(&self) -> u32 {
        self.iterations
    }

    fn scram_mock_authentication_secret(&self) -> Option<[u8; 32]> {
        Some(self.secret)
    }

    fn add_user(&mut self, username: &str, password: &str) -> Result<()> {
        self.users.insert(
            username.to_owned(),
            ScramCredentials::from_password(username.to_owned(), password, self.iterations),
        );
        Ok(())
    }

    fn remove_user(&mut self, username: &str) -> Result<bool> {
        Ok(self.users.remove(username).is_some())
    }

    fn update_password(&mut self, username: &str, new_password: &str) -> Result<()> {
        self.users
            .get_mut(username)
            .ok_or_else(|| Error::authentication("User not found"))?
            .update_password(new_password);
        Ok(())
    }

    fn user_exists(&self, username: &str) -> bool {
        self.users.contains_key(username)
    }

    fn list_users(&self) -> Vec<String> {
        self.users.keys().cloned().collect()
    }
}

/// The server's synthetic-credential derivation, reimplemented independently:
/// concatenated HMAC-SHA256(secret, domain || 0x00 || username || 0x00 || ctr).
fn synthetic_block(secret: &[u8; 32], domain: &[u8], username: &str, output_len: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(output_len);
    let mut counter = 0_u32;
    while output.len() < output_len {
        let mut input = Vec::new();
        input.extend_from_slice(domain);
        input.push(0);
        input.extend_from_slice(username.as_bytes());
        input.push(0);
        input.extend_from_slice(&counter.to_be_bytes());
        let block = scram_hmac_sha256(secret, &input);
        let needed = output_len - output.len();
        output.extend_from_slice(&block[..needed.min(block.len())]);
        counter += 1;
    }
    output
}

fn configured(iterations: u32) -> (SharedPasswordStore, Arc<AuthManager>) {
    let store = SharedPasswordStore::new(InMemoryPasswordStore::with_iterations(iterations));
    store.add_user("alice", "correct-password").expect("register user");
    let auth = Arc::new(AuthManager::with_password_store(AuthMethod::ScramSha256, store.clone()));
    (store, auth)
}

/// The bug itself: the unknown name used to stop one message earlier, with a
/// different SQLSTATE and a message that named the condition.
#[tokio::test]
async fn unknown_user_and_wrong_password_have_the_same_failure_exchange() {
    let (_, auth) = configured(4096);
    let empty = SharedPasswordStore::new(InMemoryPasswordStore::with_iterations(4096));
    let empty_auth = Arc::new(AuthManager::with_password_store(AuthMethod::ScramSha256, empty));

    // ONE name, twice: registered with a wrong password, then not registered at
    // all. Nothing the client can observe may differ between these two.
    let known = exchange(Arc::clone(&auth), "alice", "wrong-password").await;
    let absent = exchange(empty_auth, "alice", "wrong-password").await;
    // And a name that was never registered anywhere.
    let unknown = exchange(auth, "missing-user", "wrong-password").await;

    assert_eq!(known.auth_messages, [10, 11]);
    assert_eq!(
        absent.auth_messages, known.auth_messages,
        "an early error enumerates accounts"
    );
    assert_eq!(unknown.auth_messages, known.auth_messages);
    assert_eq!(known.error.get(&'C').map(String::as_str), Some("28P01"));
    assert_eq!(
        absent.error, known.error,
        "for one name, existing and not existing must be byte-identical"
    );

    // Across two different names the ONLY difference may be the name the client
    // itself put in the startup packet — PostgreSQL echoes it too, and the peer
    // already knows it.
    assert_eq!(unknown.error.get(&'C'), known.error.get(&'C'));
    assert_eq!(
        unknown.error.keys().collect::<Vec<_>>(),
        known.error.keys().collect::<Vec<_>>(),
        "the same error fields must be present either way"
    );
    let renamed: BTreeMap<char, String> = unknown
        .error
        .iter()
        .map(|(field, value)| (*field, value.replace("missing-user", "alice")))
        .collect();
    assert_eq!(
        renamed, known.error,
        "beyond the echoed user name nothing may distinguish the two"
    );
    assert!(
        known
            .error
            .values()
            .all(|field| !field.contains("User not found") && !field.contains("does not exist")),
        "the terminal error must not echo account-existence information"
    );
    // EXACT, not `contains`: `Error::authentication` is an `Error::Protocol`
    // whose Display prepends `Protocol error: `, and that prefix used to ship
    // to the client in front of PostgreSQL's own wording. The wire message is
    // now carried separately from the logged error, so `M` is byte-for-byte
    // what libpq prints.
    assert_eq!(
        known.error.get(&'M').map(String::as_str),
        Some("password authentication failed for user \"alice\""),
        "the rejection must be PostgreSQL's own wording, with no wrapper prefix"
    );
    assert_eq!(
        unknown.error.get(&'M').map(String::as_str),
        Some("password authentication failed for user \"missing-user\"")
    );
    assert_eq!(
        known.error.get(&'S').map(String::as_str),
        Some("FATAL"),
        "a startup rejection is FATAL"
    );
    assert!(
        !known.ready && !absent.ready && !unknown.ready,
        "failed authentication cannot reach ReadyForQuery"
    );
}

/// Mock authentication is only uniform if the synthetic salt behaves like a
/// real one: stable per name, distinct between names, with a fresh nonce.
#[tokio::test]
async fn mock_salts_are_stable_per_user_and_follow_the_store_iteration_policy() {
    let (_, auth) = configured(8192);
    let a = exchange(Arc::clone(&auth), "missing-user", "wrong-password").await;
    let b = exchange(Arc::clone(&auth), "missing-user", "wrong-password").await;
    let c = exchange(Arc::clone(&auth), "another-user", "wrong-password").await;
    let known = exchange(auth, "alice", "wrong-password").await;

    assert_eq!(a.auth_messages, [10, 11]);
    assert_eq!(a.salt.len(), known.salt.len());
    assert_eq!(a.salt, b.salt, "a random salt per attempt is another account oracle");
    assert_ne!(a.salt, c.salt, "one shared salt identifies all unknown accounts");
    assert_ne!(a.nonce, b.nonce, "the server nonce must remain fresh");
    assert_eq!(a.iterations, 8192, "the store's iteration policy must be advertised");
    assert_eq!(a.iterations, known.iterations);
}

/// The fix must not turn into "SCRAM never authenticates anyone", and removing
/// an account must make it indistinguishable from one that never existed.
#[tokio::test]
async fn valid_credentials_still_authenticate_and_deleted_users_are_denied() {
    let (store, auth) = configured(4096);
    let missing = exchange(Arc::clone(&auth), "missing-user", "correct-password").await;
    assert_eq!(missing.auth_messages, [10, 11]);
    assert!(!missing.error.is_empty());

    // Baseline: the account EXISTS and the password is wrong.
    let wrong = exchange(Arc::clone(&auth), "alice", "wrong-password").await;
    assert_eq!(wrong.auth_messages, [10, 11]);

    let valid = exchange(Arc::clone(&auth), "alice", "correct-password").await;
    assert_eq!(valid.auth_messages, [10, 11, 12, 0]);
    assert!(valid.ready && valid.error.is_empty());

    assert!(store.remove_user("alice").expect("remove user"));
    let deleted = exchange(auth, "alice", "correct-password").await;
    assert_eq!(deleted.auth_messages, missing.auth_messages);
    assert_eq!(
        deleted.error, wrong.error,
        "deleting the account must change nothing the client can observe"
    );
    assert!(!deleted.ready);
}

/// `must_fail` is a MANDATORY rejection, not a verification result: a proof
/// forged against the synthetic credential verifies cryptographically (the
/// positive control proves the forgery is genuinely valid) and must still be
/// refused for a name that does not exist.
#[tokio::test]
async fn proof_matching_synthetic_credentials_still_cannot_authenticate() {
    let secret = [0x71; 32];
    let username = "missing-user";
    let client_key = synthetic_block(&secret, b"client", username, 32);
    let matching_credentials = ScramCredentials {
        username: username.to_owned(),
        salt: synthetic_block(&secret, b"salt", username, 16),
        iterations: 4096,
        stored_key: scram_h(&client_key),
        server_key: synthetic_block(&secret, b"server", username, 32),
    };
    let mut users = HashMap::new();
    users.insert(username.to_owned(), matching_credentials);
    // Positive control: a store that REALLY holds this credential (the secret
    // travels through the backend's `scram_mock_authentication_secret`).
    let positive_store = SharedPasswordStore::new(PolicyStore {
        users,
        iterations: 4096,
        secret,
    });
    let positive_auth = Arc::new(AuthManager::with_password_store(
        AuthMethod::ScramSha256,
        positive_store,
    ));
    let positive = exchange_with_mock_secret(positive_auth, username, "unused", Some(&secret), None).await;
    assert_eq!(positive.auth_messages, [10, 11, 12, 0]);
    assert!(positive.ready, "positive control must pass cryptographic verification");

    // Same forged proof, same secret — but the account does not exist.
    let absent_store = SharedPasswordStore::with_mock_authentication_secret(InMemoryPasswordStore::new(), secret);
    let absent_auth = Arc::new(AuthManager::with_password_store(AuthMethod::ScramSha256, absent_store));
    let forged = exchange_with_mock_secret(absent_auth, username, "unused", Some(&secret), None).await;

    assert_eq!(forged.auth_messages, [10, 11]);
    assert_eq!(forged.error.get(&'C').map(String::as_str), Some("28P01"));
    assert!(!forged.ready, "must_fail must override a cryptographically valid proof");
}

/// Malformed input stays a protocol violation — but the SAME one for both,
/// decided before any credential is consulted.
#[tokio::test]
async fn malformed_proofs_remain_uniform_protocol_violations() {
    let (_, auth) = configured(4096);
    let known = exchange_with_mock_secret(Arc::clone(&auth), "alice", "unused", None, Some("***")).await;
    let unknown = exchange_with_mock_secret(auth, "missing-user", "unused", None, Some("***")).await;

    assert_eq!(known.auth_messages, [10, 11]);
    assert_eq!(unknown.auth_messages, known.auth_messages);
    assert_eq!(known.error.get(&'C').map(String::as_str), Some("08P01"));
    assert_eq!(unknown.error, known.error);
    assert!(!known.ready && !unknown.ready);
}

/// The cleartext arm had the SAME oracle in a different currency: a known name
/// cost a full PBKDF2 (>= 4096 iterations) inside `ScramCredentials::
/// verify_password`, an unknown one a single SHA-256 of the submitted password.
/// That is milliseconds of difference on an unauthenticated code path — a timing
/// enumeration, even though the wire bytes already matched. Both names now run
/// against a credential (real or synthetic), so the work is the same and so is
/// everything the client can see.
#[tokio::test]
async fn cleartext_absent_and_wrong_password_share_one_rejection() {
    let store = SharedPasswordStore::new(InMemoryPasswordStore::with_iterations(4096));
    store.add_user("alice", "correct-password").expect("register user");
    let auth = Arc::new(AuthManager::with_password_store(AuthMethod::CleartextPassword, store));
    // The same name, in a store where it does not exist at all.
    let empty_auth = Arc::new(AuthManager::with_password_store(
        AuthMethod::CleartextPassword,
        SharedPasswordStore::new(InMemoryPasswordStore::with_iterations(4096)),
    ));

    let known = exchange(Arc::clone(&auth), "alice", "wrong-password").await;
    let absent = exchange(empty_auth, "alice", "wrong-password").await;
    let unknown = exchange(Arc::clone(&auth), "missing-user", "wrong-password").await;

    // AuthenticationCleartextPassword (3) and then the error — no extra or
    // missing message either way, and never a ReadyForQuery after a FATAL.
    assert_eq!(known.auth_messages, [3]);
    assert_eq!(absent.auth_messages, known.auth_messages);
    assert_eq!(unknown.auth_messages, known.auth_messages);
    assert_eq!(known.error.get(&'C').map(String::as_str), Some("28P01"));
    assert_eq!(
        absent.error, known.error,
        "for one name, existing and not existing must be byte-identical"
    );
    assert_eq!(
        known.error.get(&'M').map(String::as_str),
        Some("password authentication failed for user \"alice\""),
        "no `Protocol error: ` prefix may reach the client"
    );

    // Across two different names only the echoed name may differ (same trick as
    // the SCRAM test above).
    let renamed: BTreeMap<char, String> = unknown
        .error
        .iter()
        .map(|(field, value)| (*field, value.replace("missing-user", "alice")))
        .collect();
    assert_eq!(renamed, known.error);
    assert!(!known.ready && !absent.ready && !unknown.ready);

    // Positive control: cleartext must still authenticate the real password —
    // otherwise the uniformity above would be uniform failure.
    let valid = exchange(auth, "alice", "correct-password").await;
    assert_eq!(valid.auth_messages, [3, 0]);
    assert!(valid.ready && valid.error.is_empty());
}
