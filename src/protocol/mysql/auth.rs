//! Password authentication for the MySQL listener.
//!
//! The listener's plugin is `caching_sha2_password`, the default of MySQL 8
//! clients. The server never stores a MySQL-specific verifier: a login is
//! checked against the same accounts the PostgreSQL wire uses (the server's
//! password store and every SQL role with `LOGIN` and a password), which keep
//! SCRAM-SHA-256 verifiers. That check needs the password itself, so the
//! exchange is the plugin's *full authentication*:
//!
//! * over TLS or the Unix socket the client sends the password as it is;
//! * over plain TCP the client asks for the server's RSA public key, and sends
//!   the password XOR-ed with the handshake scramble and encrypted with
//!   RSA-OAEP. The key pair is generated when the server starts (or loaded from
//!   a file) and never leaves memory.
//!
//! After one full authentication the server keeps, in memory only,
//! `SHA256(SHA256(password))` for that account: the value the plugin's *fast*
//! path verifies a scramble against. The entry is tied to the stored verifier
//! it was derived under, so it stops matching the moment the account's password
//! changes or the account is dropped.
//!
//! A wrong password and an unknown account get the same answer after the same
//! work: one key derivation and the same packets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rsa::{
    KeySize, OaepPrivateDecryptingKey, OaepPublicEncryptingKey, PrivateDecryptingKey, PublicEncryptingKey,
    OAEP_SHA1_MGF1SHA1,
};
use base64::Engine as _;
use bytes::{BufMut, BytesMut};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::protocol::postgres::auth::{AuthManager, AuthMethod};
use crate::protocol::postgres::password_store::{role_login_credentials, ScramCredentials};
use crate::EmbeddedDatabase;

/// The plugin this listener authenticates with.
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";
/// The plugin a client may use to send the password as it is (secure transports only).
pub const MYSQL_CLEAR_PASSWORD: &str = "mysql_clear_password";

/// `AuthMoreData` status: the scramble was verified from the cache.
pub const FAST_AUTH_SUCCESS: u8 = 0x03;
/// `AuthMoreData` status: the client must send the password.
pub const PERFORM_FULL_AUTHENTICATION: u8 = 0x04;
/// What a client sends to ask for the server's RSA public key.
pub const REQUEST_PUBLIC_KEY: u8 = 0x02;

struct RsaKeys {
    private: OaepPrivateDecryptingKey,
    public_pem: String,
}

struct FastEntry {
    /// The stored verifier the entry was derived under.
    verifier: String,
    /// `SHA256(SHA256(password))`.
    double_sha: [u8; 32],
}

/// The MySQL listener's authenticator: the server's accounts, the RSA key pair
/// of the public-key exchange and the fast-path cache.
pub struct MysqlAuth {
    manager: Arc<AuthManager>,
    rsa: Option<RsaKeys>,
    fast: parking_lot::Mutex<HashMap<String, FastEntry>>,
    full_verifications: AtomicU64,
    fast_successes: AtomicU64,
    failed_logins: AtomicU64,
}

impl std::fmt::Debug for MysqlAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlAuth")
            .field("method", &self.manager.method())
            .field("rsa", &self.rsa.is_some())
            .finish()
    }
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    let mut rest = encoded.as_str();
    while !rest.is_empty() {
        let (line, tail) = rest.split_at(rest.len().min(64));
        out.push_str(line);
        out.push('\n');
        rest = tail;
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn pem_body(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = pem.get(start..)?.find(&end)? + start;
    let body: String = pem.get(start..stop)?.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

fn rsa_keys(private: PrivateDecryptingKey) -> Option<RsaKeys> {
    if private.key_size_bits() < 2048 {
        return None;
    }
    let public_der = AsDer::<aws_lc_rs::encoding::PublicKeyX509Der<'_>>::as_der(&private.public_key()).ok()?;
    let public_pem = pem_wrap("PUBLIC KEY", public_der.as_ref());
    let private = OaepPrivateDecryptingKey::new(private).ok()?;
    Some(RsaKeys { private, public_pem })
}

impl MysqlAuth {
    /// The authenticator for a listener that shares `manager` with the
    /// PostgreSQL wire, with a freshly generated 2048-bit RSA key pair.
    /// `None` under `trust`: that listener asks for nothing.
    pub fn for_manager(manager: Arc<AuthManager>) -> Option<Arc<Self>> {
        if manager.method() == AuthMethod::Trust {
            return None;
        }
        let rsa = PrivateDecryptingKey::generate(KeySize::Rsa2048).ok().and_then(rsa_keys);
        if rsa.is_none() {
            tracing::warn!(
                "MySQL authentication: no RSA key pair could be generated; password logins need TLS or the Unix socket"
            );
        }
        Some(Arc::new(Self::build(manager, rsa)))
    }

    /// As [`Self::for_manager`], with the RSA private key read from a PEM file
    /// (PKCS#8, `-----BEGIN PRIVATE KEY-----`, 2048 bits or more).
    pub fn for_manager_with_rsa_key(manager: Arc<AuthManager>, pem: &str) -> crate::Result<Option<Arc<Self>>> {
        if manager.method() == AuthMethod::Trust {
            return Ok(None);
        }
        let der = pem_body(pem, "PRIVATE KEY").ok_or_else(|| {
            crate::Error::config(
                "the MySQL RSA private key must be a PKCS#8 PEM file (-----BEGIN PRIVATE KEY-----); \
                 convert a PKCS#1 key with: openssl pkcs8 -topk8 -nocrypt -in key.pem -out key.pk8.pem",
            )
        })?;
        let der = Zeroizing::new(der);
        let private = PrivateDecryptingKey::from_pkcs8(&der)
            .map_err(|e| crate::Error::config(format!("the MySQL RSA private key was rejected: {e}")))?;
        let rsa = rsa_keys(private)
            .ok_or_else(|| crate::Error::config("the MySQL RSA private key must be an RSA key of 2048 bits or more"))?;
        Ok(Some(Arc::new(Self::build(manager, Some(rsa)))))
    }

    /// The authenticator a server's MySQL listeners share: `None` when there
    /// is no listener or the method is `trust`; the RSA key read from
    /// `rsa_key_path` when one is given, else a generated one.
    pub fn for_listener(
        manager: &Arc<AuthManager>,
        rsa_key_path: Option<&std::path::Path>,
        listening: bool,
    ) -> crate::Result<Option<Arc<Self>>> {
        if !listening {
            return Ok(None);
        }
        match rsa_key_path {
            Some(path) => {
                let pem = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
                    crate::Error::config(format!(
                        "cannot read the MySQL RSA private key '{}': {e}",
                        path.display()
                    ))
                })?);
                Self::for_manager_with_rsa_key(Arc::clone(manager), &pem)
            }
            None => Ok(Self::for_manager(Arc::clone(manager))),
        }
    }

    fn build(manager: Arc<AuthManager>, rsa: Option<RsaKeys>) -> Self {
        Self {
            manager,
            rsa,
            fast: parking_lot::Mutex::new(HashMap::new()),
            full_verifications: AtomicU64::new(0),
            fast_successes: AtomicU64::new(0),
            failed_logins: AtomicU64::new(0),
        }
    }

    /// The PostgreSQL-side method the accounts are kept under.
    pub fn method(&self) -> AuthMethod {
        self.manager.method()
    }

    /// The server's RSA public key, as the PEM text a client asks for.
    pub fn public_key_pem(&self) -> Option<&str> {
        self.rsa.as_ref().map(|keys| keys.public_pem.as_str())
    }

    /// How many passwords were checked against the stored accounts: one per
    /// full authentication, whatever its outcome.
    pub fn full_verifications(&self) -> u64 {
        self.full_verifications.load(Ordering::Relaxed)
    }

    /// How many logins were accepted from the cache, without a key derivation.
    pub fn fast_successes(&self) -> u64 {
        self.fast_successes.load(Ordering::Relaxed)
    }

    /// How many logins were refused.
    pub fn failed_logins(&self) -> u64 {
        self.failed_logins.load(Ordering::Relaxed)
    }

    pub(crate) fn note_failed_login(&self) {
        self.failed_logins.fetch_add(1, Ordering::Relaxed);
    }

    /// The credential `user` logs in with: the server's own account of that
    /// name, else the SQL role's. Read for every name.
    fn credentials(&self, database: &EmbeddedDatabase, user: &str) -> (Option<ScramCredentials>, Option<ScramCredentials>) {
        let role = database
            .storage
            .catalog()
            .get_role(user)
            .ok()
            .flatten()
            .as_ref()
            .and_then(role_login_credentials);
        let stored = self.manager.password_store().and_then(|store| store.get_credentials(user));
        (stored, role)
    }

    /// Check a password against the stored accounts. One key derivation is paid
    /// whether the account exists or not, and an empty password is never right.
    pub(crate) fn verify(&self, database: &EmbeddedDatabase, user: &str, password: &str) -> bool {
        self.full_verifications.fetch_add(1, Ordering::Relaxed);
        let (_, role) = self.credentials(database, user);
        let verified = if self.manager.method() == AuthMethod::Md5 {
            self.manager.verify_basic(user, password)
        } else {
            self.manager
                .verify_cleartext_with(user, password, role)
                .unwrap_or(false)
        };
        verified & !password.is_empty()
    }

    /// Keep what the fast path needs for `user`, after a full authentication
    /// that succeeded with `password`.
    pub(crate) fn remember(&self, database: &EmbeddedDatabase, user: &str, password: &str) {
        let (stored, role) = self.credentials(database, user);
        let Some(credentials) = stored.or(role) else {
            return;
        };
        let double_sha: [u8; 32] = Sha256::digest(Sha256::digest(password.as_bytes())).into();
        self.fast.lock().insert(
            user.to_string(),
            FastEntry {
                verifier: credentials.to_verifier(),
                double_sha,
            },
        );
    }

    /// The fast path: is `scramble` the answer to `nonce` for the password
    /// `user` last logged in with, and is that still the account's password?
    pub(crate) fn fast_path(&self, database: &EmbeddedDatabase, user: &str, scramble: &[u8], nonce: &[u8]) -> bool {
        // The same hashing for a name with an entry and a name without one.
        let (double_sha, cached_verifier) = match self.fast.lock().get(user) {
            Some(entry) => (entry.double_sha, Some(entry.verifier.clone())),
            None => ([0u8; 32], None),
        };
        let mut hasher = Sha256::new();
        hasher.update(double_sha);
        hasher.update(nonce);
        let mask = hasher.finalize();
        let candidate: Vec<u8> = scramble.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect();
        let matches: bool = scramble.len() == 32 && bool::from(Sha256::digest(&candidate).as_slice().ct_eq(&double_sha));

        let (stored, role) = self.credentials(database, user);
        let current_verifier = stored.or(role).map(|credentials| credentials.to_verifier());
        let still_current = cached_verifier.is_some() && cached_verifier == current_verifier;
        if cached_verifier.is_some() && !still_current {
            self.fast.lock().remove(user);
        }
        let accepted = matches && still_current;
        if accepted {
            self.fast_successes.fetch_add(1, Ordering::Relaxed);
        }
        accepted
    }

    /// The password inside an RSA-encrypted reply: decrypted, un-XOR-ed with
    /// the scramble, without its terminator. `None` when it cannot be read.
    pub(crate) fn decrypt_password(&self, ciphertext: &[u8], nonce: &[u8]) -> Option<Zeroizing<String>> {
        let keys = self.rsa.as_ref()?;
        let mut buffer = Zeroizing::new(vec![0u8; keys.private.min_output_size()]);
        let plain = keys
            .private
            .decrypt(&OAEP_SHA1_MGF1SHA1, ciphertext, &mut buffer, None)
            .ok()?;
        if nonce.is_empty() {
            return None;
        }
        let mut clear = Zeroizing::new(Vec::with_capacity(plain.len()));
        for (byte, mask) in plain.iter().zip(nonce.iter().cycle()) {
            clear.push(byte ^ mask);
        }
        if clear.last() == Some(&0) {
            clear.pop();
        }
        String::from_utf8(clear.to_vec()).ok().map(Zeroizing::new)
    }

    /// The length of an RSA-encrypted password under this server's key.
    pub(crate) fn rsa_ciphertext_len(&self) -> Option<usize> {
        self.rsa.as_ref().map(|keys| keys.private.key_size_bytes())
    }
}

/// The client's side of the public-key exchange: the password, terminated,
/// XOR-ed with the scramble and encrypted with RSA-OAEP under the server's
/// public key (PEM). Public protocol; clients and tests use it.
pub fn rsa_encrypt_password(public_key_pem: &str, password: &str, nonce: &[u8]) -> Option<Vec<u8>> {
    if nonce.is_empty() {
        return None;
    }
    let der = pem_body(public_key_pem, "PUBLIC KEY")?;
    let key = OaepPublicEncryptingKey::new(PublicEncryptingKey::from_der(&der).ok()?).ok()?;
    let mut clear: Vec<u8> = password.as_bytes().to_vec();
    clear.push(0);
    for (byte, mask) in clear.iter_mut().zip(nonce.iter().cycle()) {
        *byte ^= mask;
    }
    let mut out = vec![0u8; key.ciphertext_size()];
    let written = key.encrypt(&OAEP_SHA1_MGF1SHA1, &clear, &mut out, None).ok()?.len();
    out.truncate(written);
    Some(out)
}

/// `AuthSwitchRequest`: continue with `caching_sha2_password` and this scramble.
pub(crate) fn auth_switch_request(nonce: &[u8; 20]) -> BytesMut {
    let mut p = BytesMut::new();
    p.put_u8(0xFE);
    p.put_slice(CACHING_SHA2_PASSWORD.as_bytes());
    p.put_u8(0);
    p.put_slice(nonce);
    p.put_u8(0);
    p
}

/// `AuthMoreData` carrying `data`.
pub(crate) fn auth_more_data(data: &[u8]) -> BytesMut {
    let mut p = BytesMut::with_capacity(data.len() + 1);
    p.put_u8(0x01);
    p.put_slice(data);
    p
}

/// The message of every refused login: MySQL's own, the same for a wrong
/// password and for an account that does not exist.
pub(crate) fn access_denied_message(user: &str, used_password: bool) -> String {
    let shown: String = user.chars().take(80).collect();
    format!(
        "Access denied for user '{shown}'@'%' (using password: {})",
        if used_password { "YES" } else { "NO" }
    )
}

/// The message of a login refused because of the transport it came over.
pub(crate) const NEEDS_SECURE_TRANSPORT: &str =
    "Access denied: this server could not exchange the password safely on a connection without TLS. \
     Connect with TLS or over the Unix socket, or let the client request the server's RSA public key \
     (--get-server-public-key, allowPublicKeyRetrieval=true)";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::protocol::postgres::password_store::{InMemoryPasswordStore, SharedPasswordStore};

    fn auth_with(user: &str, password: &str) -> Arc<MysqlAuth> {
        let store = InMemoryPasswordStore::new();
        let shared = SharedPasswordStore::new(store);
        shared.add_user(user, password).unwrap();
        let manager = Arc::new(AuthManager::with_password_store(AuthMethod::ScramSha256, shared));
        MysqlAuth::for_manager(manager).unwrap()
    }

    #[test]
    fn trust_has_no_authenticator() {
        assert!(MysqlAuth::for_manager(Arc::new(AuthManager::new(AuthMethod::Trust))).is_none());
    }

    #[test]
    fn the_password_survives_the_public_key_exchange() {
        let auth = auth_with("app", "s3cret");
        let nonce = [7u8; 20];
        let pem = auth.public_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        for password in ["s3cret", "", "a password longer than the twenty bytes of the scramble"] {
            let sealed = rsa_encrypt_password(pem, password, &nonce).unwrap();
            assert_eq!(sealed.len(), auth.rsa_ciphertext_len().unwrap());
            assert!(auth.rsa_ciphertext_len().unwrap() >= 256);
            assert_eq!(auth.decrypt_password(&sealed, &nonce).unwrap().as_str(), password);
        }
        assert!(auth.decrypt_password(&[1u8; 256], &nonce).is_none());
    }

    #[test]
    fn a_private_key_is_loaded_from_pkcs8_pem_and_others_are_refused() {
        let key = PrivateDecryptingKey::generate(KeySize::Rsa2048).unwrap();
        let der = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der<'_>>::as_der(&key).unwrap();
        let pem = pem_wrap("PRIVATE KEY", der.as_ref());
        let manager = Arc::new(AuthManager::with_scram_store(AuthMethod::ScramSha256));
        let auth = MysqlAuth::for_manager_with_rsa_key(Arc::clone(&manager), &pem)
            .unwrap()
            .unwrap();
        let sealed = rsa_encrypt_password(auth.public_key_pem().unwrap(), "pw", &[1u8; 20]).unwrap();
        assert_eq!(auth.decrypt_password(&sealed, &[1u8; 20]).unwrap().as_str(), "pw");

        let refused = MysqlAuth::for_manager_with_rsa_key(manager, "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n");
        assert!(refused.unwrap_err().to_string().contains("PKCS#8"));
    }

    #[test]
    fn the_fast_path_needs_one_full_authentication_and_the_same_stored_password() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let auth = auth_with("app", "s3cret");
        let nonce = [9u8; 20];
        let scramble = crate::protocol::mysql::handler::compute_caching_sha2_auth("s3cret", &nonce);
        assert!(!auth.fast_path(&db, "app", &scramble, &nonce), "nothing cached yet");

        assert!(auth.verify(&db, "app", "s3cret"));
        auth.remember(&db, "app", "s3cret");
        assert!(auth.fast_path(&db, "app", &scramble, &nonce));
        let wrong = crate::protocol::mysql::handler::compute_caching_sha2_auth("other", &nonce);
        assert!(!auth.fast_path(&db, "app", &wrong, &nonce));
        assert!(!auth.fast_path(&db, "nobody", &scramble, &nonce));

        // The password changes: the cached entry is dropped, not honoured.
        auth.manager.password_store().unwrap().update_password("app", "new").unwrap();
        assert!(!auth.fast_path(&db, "app", &scramble, &nonce));
        assert!(auth.fast.lock().is_empty());
    }

    #[test]
    fn an_empty_password_and_an_unknown_user_are_refused_after_the_same_work() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let auth = auth_with("app", "s3cret");
        assert!(!auth.verify(&db, "app", ""));
        assert!(!auth.verify(&db, "app", "wrong"));
        assert!(!auth.verify(&db, "nobody", "s3cret"));
        assert_eq!(auth.full_verifications(), 3);
    }
}
