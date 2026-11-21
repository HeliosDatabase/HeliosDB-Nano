//! The login exchange of a MySQL connection: `caching_sha2_password` (fast
//! path, full authentication over a secure transport, full authentication
//! through the RSA public-key exchange), `mysql_clear_password` on a secure
//! transport, the switch away from any other plugin, and `COM_CHANGE_USER`.
//!
//! The accounts and the cryptography are `super::super::auth`.

use super::*;
use crate::protocol::mysql::auth::{
    self, MysqlAuth, CACHING_SHA2_PASSWORD, FAST_AUTH_SUCCESS, MYSQL_CLEAR_PASSWORD, NEEDS_SECURE_TRANSPORT,
    PERFORM_FULL_AUTHENTICATION, REQUEST_PUBLIC_KEY,
};
use zeroize::Zeroizing;

/// How a login attempt ended.
enum LoginOutcome {
    Accepted,
    /// Wrong password or no such account: one answer for both.
    Denied { used_password: bool },
    /// The password could not be exchanged safely on this transport.
    NeedsSecureTransport,
}

fn strip_terminator(mut bytes: Vec<u8>) -> Zeroizing<String> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    bytes.iter_mut().for_each(|b| *b = 0);
    Zeroizing::new(text)
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> MySqlHandler<S> {
    async fn read_auth_packet(&mut self) -> Result<Vec<u8>> {
        let (seq, payload) = read_packet(&mut self.stream).await?;
        self.seq = seq.wrapping_add(1);
        Ok(payload.to_vec())
    }

    fn fresh_nonce(&mut self) {
        use rand::Rng;
        let mut nonce = [0u8; 20];
        rand::thread_rng().fill(&mut nonce);
        // The scramble is NUL-terminated on the wire: no zero byte inside it.
        for byte in &mut nonce {
            if *byte == 0 {
                *byte = 0x5a;
            }
        }
        self.auth_seed = nonce;
    }

    /// Check a password off the async worker (it runs a key derivation).
    fn verify_password(&self, auth: &MysqlAuth, user: &str, password: &str) -> bool {
        crate::runtime_offload::offload_statement(|| auth.verify(&self.database, user, password))
    }

    /// Run the exchange for `user` from the plugin and first answer the
    /// client sent. Writes the intermediate packets; the caller writes the
    /// final OK or ERR.
    async fn login_exchange(
        &mut self,
        auth: &Arc<MysqlAuth>,
        user: &str,
        plugin: Option<&str>,
        first_answer: Vec<u8>,
        force_switch: bool,
    ) -> Result<LoginOutcome> {
        let secure = self.transport_secure;
        if !force_switch && plugin == Some(MYSQL_CLEAR_PASSWORD) {
            if !secure {
                return Ok(LoginOutcome::NeedsSecureTransport);
            }
            let password = strip_terminator(first_answer);
            let accepted = self.verify_password(auth, user, &password);
            if accepted {
                auth.remember(&self.database, user, &password);
            }
            return Ok(if accepted {
                LoginOutcome::Accepted
            } else {
                LoginOutcome::Denied {
                    used_password: !password.is_empty(),
                }
            });
        }

        let scramble = if !force_switch && plugin == Some(CACHING_SHA2_PASSWORD) {
            first_answer
        } else {
            // Any other plugin (mysql_native_password included, which this
            // server does not offer), or none named: continue with ours, on a
            // scramble of its own.
            self.fresh_nonce();
            let request = auth::auth_switch_request(&self.auth_seed);
            self.write_pkt(&request).await?;
            self.read_auth_packet().await?
        };
        let nonce = self.auth_seed;

        if scramble.is_empty() {
            // The client has no password. One is never empty here; the work
            // of a check is still paid, so this answer costs what others do.
            let _ = self.verify_password(auth, user, "");
            return Ok(LoginOutcome::Denied { used_password: false });
        }
        if auth.fast_path(&self.database, user, &scramble, &nonce) {
            self.write_pkt(&auth::auth_more_data(&[FAST_AUTH_SUCCESS])).await?;
            return Ok(LoginOutcome::Accepted);
        }

        self.write_pkt(&auth::auth_more_data(&[PERFORM_FULL_AUTHENTICATION]))
            .await?;
        let answer = self.read_auth_packet().await?;
        let password: Option<Zeroizing<String>> = if secure {
            Some(strip_terminator(answer))
        } else if answer.as_slice() == [REQUEST_PUBLIC_KEY] {
            match auth.public_key_pem() {
                Some(pem) => {
                    let pem = pem.as_bytes().to_vec();
                    self.write_pkt(&auth::auth_more_data(&pem)).await?;
                    let sealed = self.read_auth_packet().await?;
                    auth.decrypt_password(&sealed, &nonce)
                }
                None => return Ok(LoginOutcome::NeedsSecureTransport),
            }
        } else if auth.rsa_ciphertext_len() == Some(answer.len()) {
            // A client that already holds the server's public key.
            auth.decrypt_password(&answer, &nonce)
        } else {
            // The password as it is, on a connection anyone can read: refused
            // whatever it was.
            return Ok(LoginOutcome::NeedsSecureTransport);
        };
        let Some(password) = password else {
            let _ = self.verify_password(auth, user, "");
            return Ok(LoginOutcome::Denied { used_password: true });
        };
        if self.verify_password(auth, user, &password) {
            auth.remember(&self.database, user, &password);
            Ok(LoginOutcome::Accepted)
        } else {
            Ok(LoginOutcome::Denied {
                used_password: !password.is_empty(),
            })
        }
    }

    /// Send the refusal of a login and report it as the end of the connection.
    async fn refuse_login(&mut self, auth: &MysqlAuth, user: &str, outcome: &LoginOutcome) -> Result<()> {
        auth.note_failed_login();
        let message = match outcome {
            LoginOutcome::NeedsSecureTransport => NEEDS_SECURE_TRANSPORT.to_string(),
            LoginOutcome::Denied { used_password } => auth::access_denied_message(user, *used_password),
            LoginOutcome::Accepted => return Ok(()),
        };
        info!("MySQL login refused for connection {}", self.connection_id);
        self.send_error(1045, "28000", &message).await?;
        Err(MySqlError::ConnectionClosed)
    }

    /// Authenticate the handshake response: trust when the listener has no
    /// authenticator, the password exchange otherwise.
    pub(super) async fn authenticate(&mut self, hs: &HandshakeResponse) -> Result<()> {
        self.current_database = hs.database.clone();
        // Intersect capabilities
        self.capabilities = CapabilityFlags::new(self.capabilities.as_u32() & hs.capability_flags.as_u32());
        let plugin = hs.auth_plugin_name.as_deref();
        debug!("Auth user='{}' plugin={:?} db={:?}", hs.username, plugin, hs.database);

        let Some(auth) = self.auth.clone() else {
            self.username = Some(hs.username.clone());
            info!("User '{}' authenticated (trust)", hs.username);
            return Ok(());
        };
        let outcome = self
            .login_exchange(&auth, &hs.username, plugin, hs.auth_response.clone(), false)
            .await?;
        if !matches!(outcome, LoginOutcome::Accepted) {
            return self.refuse_login(&auth, &hs.username, &outcome).await;
        }
        self.username = Some(hs.username.clone());
        info!("User '{}' authenticated ({:?})", hs.username, auth.method());
        Ok(())
    }

    /// `COM_CHANGE_USER`: authenticate as another account on this connection
    /// and start its session from nothing. A refusal ends the connection.
    pub(super) async fn handle_change_user(&mut self, mut payload: Bytes) -> Result<()> {
        let user = read_null_terminated(&mut payload)?;
        let answer = if self.capabilities.has(CapabilityFlags::CLIENT_SECURE_CONNECTION) {
            if payload.is_empty() {
                return Err(MySqlError::Protocol("COM_CHANGE_USER too short".into()));
            }
            let len = payload.get_u8() as usize;
            if payload.remaining() < len {
                return Err(MySqlError::Protocol("COM_CHANGE_USER auth response truncated".into()));
            }
            payload.copy_to_bytes(len).to_vec()
        } else {
            read_null_terminated_bytes(&mut payload)?
        };
        let database = if payload.has_remaining() {
            Some(read_null_terminated(&mut payload)?).filter(|name| !name.is_empty())
        } else {
            None
        };

        if let Some(auth) = self.auth.clone() {
            // Always on a scramble of its own: the answer in the packet was
            // computed from the handshake's, which this connection already used.
            let _ = answer;
            let outcome = self.login_exchange(&auth, &user, None, Vec::new(), true).await?;
            if !matches!(outcome, LoginOutcome::Accepted) {
                return self.refuse_login(&auth, &user, &outcome).await;
            }
        }

        self.reset_session_state();
        self.username = Some(user);
        self.database
            .set_session_login_user(self.session_id, self.username.clone())?;
        self.current_database = database;
        self.bind_current_database_tenant().await?;
        self.send_ok(0, 0).await
    }
}
