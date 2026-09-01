//! Authentication correctness for the PostgreSQL wire protocol — GH#19 and GH#20.
//!
//! WHAT WAS BROKEN, and why it shipped.
//!
//! * GH#19 (`--auth md5` accepted EVERY connection, including one with no password at all).
//!   The startup auth dispatch had arms for Trust / CleartextPassword / ScramSha256 and a
//!   `_ =>` catch-all commented "Other auth methods not yet implemented" whose body was
//!   `self.authenticated = true; self.send_auth_ok()`. `AuthMethod::Md5` fell into it, so
//!   the server never even sent an MD5 challenge — which is why an EMPTY `PGPASSWORD`
//!   connected: libpq was never asked. `AuthManager::verify_md5` existed with ZERO callers
//!   and was itself broken twice over (it compared an md5 digest against a SHA-256 hash,
//!   and read a legacy user map that `--auth md5 --password` never populated).
//!
//! * GH#20 (`--auth scram-sha-256` rejected the CORRECT password, unusably). The handler
//!   built its per-connection state with `ScramAuthState::new`, which fabricates a RANDOM
//!   salt. The server advertised that random salt in `s=`, the client derived its proof
//!   from it, and the server then checked the result against a `stored_key` derived from a
//!   DIFFERENT salt chosen at `add_user` time. The comparison could never succeed for any
//!   password. A second, independent break: the handler never sent AuthenticationOk after
//!   AuthenticationSASLFinal, so even a verified proof ended in libpq's "expected
//!   authentication request from server, but received S".
//!
//! WHY THE EXISTING TESTS MISSED IT. `tests/postgres_scram_auth_tests.rs` covers every
//! SCRAM primitive in isolation — `scram_hi`, HMAC, H, the key-derivation chain,
//! credential construction — and they all pass. What no test did was complete ONE proof
//! round trip against a STORED credential, which is the only thing that exercises the
//! agreement between the advertised salt and the derived key. Testing the parts is not
//! testing the composition.
//!
//! MAINTENANCE RULE FOR THIS FILE. Every mode must assert the NEGATIVE — that a wrong
//! password is REJECTED — not merely that the right one is accepted. A happy-path-only
//! test passes against a server that accepts everybody, which is exactly the bug that
//! shipped in v4.23.0 and v4.24.0.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::protocol::postgres::auth::{
    scram_client_first_bare, scram_hi, scram_hmac_sha256, DEFAULT_SCRAM_ITERATIONS,
};
use heliosdb_nano::protocol::postgres::password_store::ScramCredentials;
use heliosdb_nano::protocol::postgres::{AuthManager, AuthMethod, ScramAuthState};

// ===========================================================================
// GH#19 — md5
// ===========================================================================

/// The PostgreSQL client-side md5 response: `"md5" + hex(md5(hex(md5(password||username)) || salt))`.
fn client_md5_response(username: &str, password: &str, salt: &[u8; 4]) -> String {
    let inner_hex = format!(
        "{:x}",
        md5::compute([password.as_bytes(), username.as_bytes()].concat())
    );
    format!(
        "md5{:x}",
        md5::compute([inner_hex.as_bytes(), salt.as_slice()].concat())
    )
}

fn md5_manager(username: &str, password: &str) -> AuthManager {
    let mut auth = AuthManager::new(AuthMethod::Md5);
    auth.add_user(username.to_string(), password.to_string());
    auth
}

#[test]
fn md5_accepts_the_correct_password() {
    let auth = md5_manager("postgres", "correctpw456");
    let salt = [0x11u8, 0x22, 0x33, 0x44];
    let response = client_md5_response("postgres", "correctpw456", &salt);
    assert!(
        auth.verify_md5_response("postgres", &response, &salt).unwrap(),
        "the correct password must authenticate under md5"
    );
}

/// The assertion that actually matters. Against the shipped v4.23.0/v4.24.0 behaviour every
/// one of these was ACCEPTED.
#[test]
fn md5_rejects_wrong_empty_and_unknown() {
    let auth = md5_manager("postgres", "correctpw456");
    let salt = [0x11u8, 0x22, 0x33, 0x44];

    let wrong = client_md5_response("postgres", "totally-wrong-xyz", &salt);
    assert!(
        !auth.verify_md5_response("postgres", &wrong, &salt).unwrap(),
        "a WRONG password must be rejected under md5"
    );

    let empty = client_md5_response("postgres", "", &salt);
    assert!(
        !auth.verify_md5_response("postgres", &empty, &salt).unwrap(),
        "an EMPTY password must be rejected under md5"
    );

    assert!(
        !auth.verify_md5_response("postgres", "", &salt).unwrap(),
        "a malformed/absent response must be rejected, not treated as a match"
    );

    let good = client_md5_response("nobody", "correctpw456", &salt);
    assert!(
        !auth.verify_md5_response("nobody", &good, &salt).unwrap(),
        "an UNKNOWN user must be rejected"
    );
}

/// The salt is what stops a captured response being replayed against a new session.
#[test]
fn md5_rejects_a_response_computed_for_a_different_salt() {
    let auth = md5_manager("postgres", "correctpw456");
    let captured = client_md5_response("postgres", "correctpw456", &[1, 2, 3, 4]);
    assert!(
        !auth.verify_md5_response("postgres", &captured, &[5, 6, 7, 8]).unwrap(),
        "a response bound to another salt must not authenticate — otherwise the challenge \
         is decorative and md5 is replayable"
    );
}

// ===========================================================================
// GH#20 — SCRAM-SHA-256, full proof round trip against a STORED credential
// ===========================================================================

/// Reproduce what a real client computes, then check the server accepts it.
///
/// This is the test whose ABSENCE let GH#20 ship: it is the only shape that exercises the
/// agreement between the salt the server ADVERTISES and the salt the stored key was
/// DERIVED from. Every primitive below already passed in isolation.
fn scram_round_trip(registered_password: &str, offered_password: &str) -> bool {
    let username = "postgres";
    // Server side: the credential as `add_user` stores it — it picks its own salt.
    let credentials =
        ScramCredentials::from_password(username.to_string(), registered_password, DEFAULT_SCRAM_ITERATIONS);

    // Server side: per-connection state built the way the wire handler builds it.
    let mut state =
        ScramAuthState::with_credentials(username.to_string(), credentials.salt.clone(), credentials.iterations);

    let client_nonce = "rOprNGfwEbeRWgbNEkqO";
    let client_first = format!("n,,n=,r={client_nonce}");
    let client_first_bare = scram_client_first_bare(&client_first).expect("bare body");
    state.set_client_nonce(client_nonce.to_string());
    state.set_client_first_message_bare(client_first_bare.to_string());

    let server_first = state.build_server_first_message().expect("server-first");

    // Client side: derive the proof from the salt/iterations the SERVER advertised.
    let salted = scram_hi(offered_password, state.salt(), state.iteration_count());
    let client_key = scram_hmac_sha256(&salted, b"Client Key");
    let stored_key = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&client_key);
        h.finalize().to_vec()
    };
    let client_final_without_proof = format!("c=biws,r={}", state.combined_nonce());
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = scram_hmac_sha256(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(client_signature.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let proof_b64 = base64_encode(&proof);

    state
        .verify_client_proof(
            &proof_b64,
            &client_final_without_proof,
            &credentials.stored_key,
            &credentials.server_key,
        )
        .is_ok()
}

fn base64_encode(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[test]
fn scram_accepts_the_correct_password() {
    assert!(
        scram_round_trip("S3cretTestPw_ExampleOnly", "S3cretTestPw_ExampleOnly"),
        "GH#20: the CORRECT password must authenticate. If this fails, the salt the server \
         advertises in server-first-message does not match the salt the stored key was \
         derived from, and NO password can ever authenticate."
    );
}

#[test]
fn scram_rejects_a_wrong_password() {
    assert!(
        !scram_round_trip("S3cretTestPw_ExampleOnly", "totally-wrong-xyz"),
        "a wrong password must be rejected — without this the accept test above could be \
         satisfied by a server that accepts everything"
    );
}

/// Pins the actual defect mechanism, so a regression is diagnosed rather than merely
/// observed: state built with a RANDOM salt cannot verify a stored credential.
#[test]
fn scram_state_must_carry_the_stored_salt_not_a_random_one() {
    let creds = ScramCredentials::from_password("postgres".to_string(), "pw", DEFAULT_SCRAM_ITERATIONS);

    let from_creds = ScramAuthState::with_credentials("postgres".to_string(), creds.salt.clone(), creds.iterations);
    assert_eq!(
        from_creds.salt(),
        creds.salt.as_slice(),
        "with_credentials must advertise the credential's own salt"
    );
    assert_eq!(from_creds.iteration_count(), creds.iterations);

    let random = ScramAuthState::new("postgres".to_string());
    assert_ne!(
        random.salt(),
        creds.salt.as_slice(),
        "ScramAuthState::new generates a random salt — this is exactly why the wire handler \
         must NOT use it to verify a stored password (GH#20). If these ever compare equal \
         the test is no longer proving anything; regenerate with a different credential."
    );
}

// ===========================================================================
// client-first-message-bare must be the client's own bytes, not a reconstruction
// ===========================================================================

/// The proof hashes this string on both sides, so reconstructing it as `n=,r={nonce}` is
/// correct only for clients that send an empty `n=`. RFC 5802 permits a non-empty username
/// field and extension fields; either would silently corrupt the AuthMessage and surface as
/// "Invalid password".
#[test]
fn client_first_bare_is_taken_verbatim_from_the_client() {
    assert_eq!(scram_client_first_bare("n,,n=,r=abc"), Some("n=,r=abc"));
    // Non-empty username — a reconstruction would produce "n=,r=abc" and break the proof.
    assert_eq!(scram_client_first_bare("n,,n=alice,r=abc"), Some("n=alice,r=abc"));
    // Extension field after the nonce must survive intact.
    assert_eq!(scram_client_first_bare("n,,n=,r=abc,a=ext"), Some("n=,r=abc,a=ext"));
    // authzid present.
    assert_eq!(scram_client_first_bare("n,a=admin,n=,r=abc"), Some("n=,r=abc"));
    // Malformed GS2 headers yield None so the caller can fall back explicitly.
    assert_eq!(scram_client_first_bare("n,,"), None);
    assert_eq!(scram_client_first_bare("garbage"), None);
}
