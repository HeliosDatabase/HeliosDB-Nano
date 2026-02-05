//! PostgreSQL authentication implementation
//!
//! Supports SCRAM-SHA-256 authentication mechanism

use sha2::{Sha256, Digest};
#[cfg(feature = "ring-crypto")]
use ring::hmac;
#[cfg(feature = "ring-crypto")]
use ring::pbkdf2;
use std::num::NonZeroU32;
use crate::Error;

/// SCRAM-SHA-256 authentication state machine
pub struct ScramAuth {
    state: ScramState,
    client_nonce: String,
    server_nonce: String,
    salt: Vec<u8>,
    iteration_count: u32,
    client_first_message_bare: String,
    server_first_message: String,
    stored_key: Vec<u8>,
    server_key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ScramState {
    Initial,
    SentChallenge,
    Complete,
}

impl ScramAuth {
    /// Create a new SCRAM authenticator with a password
    pub fn new(password: &str) -> Self {
        // Generate random salt and nonce
        let salt = Self::generate_salt();
        let server_nonce = Self::generate_nonce();

        // Derive keys from password
        let (stored_key, server_key) = Self::derive_keys(password, &salt, 4096);

        Self {
            state: ScramState::Initial,
            client_nonce: String::new(),
            server_nonce,
            salt,
            iteration_count: 4096,
            client_first_message_bare: String::new(),
            server_first_message: String::new(),
            stored_key,
            server_key,
        }
    }

    /// Process client's initial message
    ///
    /// Expected format: "n,,n=<username>,r=<client-nonce>"
    pub fn process_client_first(&mut self, client_first: &str) -> Result<Vec<u8>, Error> {
        if self.state != ScramState::Initial {
            return Err(Error::protocol("Invalid SCRAM state"));
        }

        // Parse GS2 header and client-first-message-bare
        let parts: Vec<&str> = client_first.splitn(3, ',').collect();
        if parts.len() < 3 {
            return Err(Error::protocol("Invalid SCRAM client-first-message"));
        }

        // Extract client-first-message-bare (everything after GS2 header)
        let bare_start = if let Some(pos) = client_first.find("n=") {
            pos
        } else {
            return Err(Error::protocol("Missing username in SCRAM message"));
        };
        self.client_first_message_bare = client_first.get(bare_start..).unwrap_or_default().to_string();

        // Parse client-first-message-bare attributes
        let attrs = Self::parse_attributes(&self.client_first_message_bare)?;

        // Extract client nonce
        self.client_nonce = attrs
            .get("r")
            .ok_or_else(|| Error::protocol("Missing nonce in SCRAM message"))?
            .to_string();

        // Build server-first-message
        let combined_nonce = format!("{}{}", self.client_nonce, self.server_nonce);
        let salt_base64 = base64::encode(&self.salt);

        self.server_first_message = format!(
            "r={},s={},i={}",
            combined_nonce, salt_base64, self.iteration_count
        );

        self.state = ScramState::SentChallenge;

        Ok(self.server_first_message.as_bytes().to_vec())
    }

    /// Process client's final message
    ///
    /// Expected format: "c=<channel-binding>,r=<nonce>,p=<client-proof>"
    pub fn process_client_final(&mut self, client_final: &str) -> Result<Vec<u8>, Error> {
        if self.state != ScramState::SentChallenge {
            return Err(Error::protocol("Invalid SCRAM state"));
        }

        // Parse client-final-message-without-proof
        let proof_pos = client_final
            .rfind(",p=")
            .ok_or_else(|| Error::protocol("Missing proof in SCRAM message"))?;
        let client_final_without_proof = client_final.get(..proof_pos).unwrap_or(client_final);

        // Parse attributes
        let attrs = Self::parse_attributes(client_final)?;

        // Verify nonce
        let combined_nonce = format!("{}{}", self.client_nonce, self.server_nonce);
        let client_nonce = attrs
            .get("r")
            .ok_or_else(|| Error::protocol("Missing nonce in SCRAM final message"))?;
        if client_nonce != &combined_nonce {
            return Err(Error::protocol("Nonce mismatch"));
        }

        // Get client proof
        let client_proof_b64 = attrs
            .get("p")
            .ok_or_else(|| Error::protocol("Missing proof in SCRAM final message"))?;
        let client_proof = base64::decode(client_proof_b64)
            .map_err(|_| Error::protocol("Invalid base64 in client proof"))?;

        // Compute auth message
        let auth_message = format!(
            "{},{},{}",
            self.client_first_message_bare, self.server_first_message, client_final_without_proof
        );

        // Verify client proof
        self.verify_client_proof(&client_proof, &auth_message)?;

        // Compute server signature
        let server_signature = self.compute_server_signature(&auth_message);
        let server_signature_b64 = base64::encode(&server_signature);

        // Build server-final-message
        let server_final = format!("v={}", server_signature_b64);

        self.state = ScramState::Complete;

        Ok(server_final.as_bytes().to_vec())
    }

    /// Verify the client's proof
    fn verify_client_proof(&self, client_proof: &[u8], auth_message: &str) -> Result<(), Error> {
        // Compute client signature
        let client_key = hmac::Key::new(hmac::HMAC_SHA256, &self.stored_key);
        let client_signature = hmac::sign(&client_key, auth_message.as_bytes());

        // Compute client key from proof
        let computed_client_key: Vec<u8> = client_proof.iter()
            .zip(client_signature.as_ref().iter())
            .map(|(a, b)| a ^ b)
            .collect();

        // Hash the computed client key
        let mut hasher = Sha256::new();
        hasher.update(&computed_client_key);
        let computed_stored_key = hasher.finalize();

        // Verify stored key matches
        if computed_stored_key.as_slice() != self.stored_key {
            return Err(Error::protocol("Authentication failed: invalid password"));
        }

        Ok(())
    }

    /// Compute server signature
    fn compute_server_signature(&self, auth_message: &str) -> Vec<u8> {
        let server_key = hmac::Key::new(hmac::HMAC_SHA256, &self.server_key);
        let signature = hmac::sign(&server_key, auth_message.as_bytes());
        signature.as_ref().to_vec()
    }

    /// Derive stored_key and server_key from password
    fn derive_keys(password: &str, salt: &[u8], iterations: u32) -> (Vec<u8>, Vec<u8>) {
        // Compute salted password using PBKDF2
        let mut salted_password = vec![0u8; 32]; // SHA-256 output size
        // Use minimum of 4096 iterations as per SCRAM-SHA-256 recommendation if iterations is 0
        const MIN_ITERATIONS: NonZeroU32 = match NonZeroU32::new(4096) {
            Some(n) => n,
            None => unreachable!(), // 4096 is non-zero
        };
        let iterations_non_zero = NonZeroU32::new(iterations).unwrap_or(MIN_ITERATIONS);
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations_non_zero,
            salt,
            password.as_bytes(),
            &mut salted_password,
        );

        // Compute client_key = HMAC(salted_password, "Client Key")
        let client_key_hmac = hmac::Key::new(hmac::HMAC_SHA256, &salted_password);
        let client_key = hmac::sign(&client_key_hmac, b"Client Key");

        // Compute stored_key = H(client_key)
        let mut hasher = Sha256::new();
        hasher.update(client_key.as_ref());
        let stored_key = hasher.finalize().to_vec();

        // Compute server_key = HMAC(salted_password, "Server Key")
        let server_key_hmac = hmac::Key::new(hmac::HMAC_SHA256, &salted_password);
        let server_key = hmac::sign(&server_key_hmac, b"Server Key");

        (stored_key, server_key.as_ref().to_vec())
    }

    /// Parse SCRAM attributes (key=value pairs)
    fn parse_attributes(message: &str) -> Result<std::collections::HashMap<String, String>, Error> {
        let mut attrs = std::collections::HashMap::new();

        for part in message.split(',') {
            if let Some(eq_pos) = part.find('=') {
                let key = part.get(..eq_pos).unwrap_or_default().to_string();
                let value = part.get(eq_pos + 1..).unwrap_or_default().to_string();
                attrs.insert(key, value);
            }
        }

        Ok(attrs)
    }

    /// Generate a random nonce
    fn generate_nonce() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let nonce: [u8; 24] = rng.gen();
        base64::encode(&nonce)
    }

    /// Generate a random salt
    fn generate_salt() -> Vec<u8> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut salt = vec![0u8; 16];
        rng.fill(&mut salt[..]);
        salt
    }

    /// Check if authentication is complete
    pub fn is_complete(&self) -> bool {
        self.state == ScramState::Complete
    }
}

/// Simple base64 encoding/decoding wrapper
mod base64 {
    pub fn encode(data: &[u8]) -> String {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut encoder = base64_encode::Encoder::new(&mut buf);
            // SAFETY: Base64 encoding to Vec<u8> cannot fail as Vec has unlimited capacity
            // and io::Write for Vec never returns Err. Using unwrap_or_default as a fallback.
            let _ = encoder.write_all(data);
        }
        // SAFETY: Base64 output uses only ASCII characters (A-Za-z0-9+/=),
        // which are all valid single-byte UTF-8. Using unwrap_or_default as fallback.
        String::from_utf8(buf).unwrap_or_default()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        use std::io::Read;
        let mut decoder = base64_decode::Decoder::new(s.as_bytes());
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf)?;
        Ok(buf)
    }

    mod base64_encode {
        use std::io::{self, Write};

        pub struct Encoder<W: Write> {
            writer: W,
            buffer: [u8; 3],
            buffer_len: usize,
        }

        impl<W: Write> Encoder<W> {
            pub fn new(writer: W) -> Self {
                Self {
                    writer,
                    buffer: [0; 3],
                    buffer_len: 0,
                }
            }

            #[allow(clippy::indexing_slicing)] // SAFETY: chunk length is checked by match arms (1/2/3), CHARS indexed by 6-bit values (0-63) into [u8; 64]
            fn encode_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
                const CHARS: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

                match chunk.len() {
                    3 => {
                        let b1 = (chunk[0] >> 2) as usize;
                        let b2 = (((chunk[0] & 0x03) << 4) | (chunk[1] >> 4)) as usize;
                        let b3 = (((chunk[1] & 0x0F) << 2) | (chunk[2] >> 6)) as usize;
                        let b4 = (chunk[2] & 0x3F) as usize;
                        self.writer
                            .write_all(&[CHARS[b1], CHARS[b2], CHARS[b3], CHARS[b4]])?;
                    }
                    2 => {
                        let b1 = (chunk[0] >> 2) as usize;
                        let b2 = (((chunk[0] & 0x03) << 4) | (chunk[1] >> 4)) as usize;
                        let b3 = ((chunk[1] & 0x0F) << 2) as usize;
                        self.writer
                            .write_all(&[CHARS[b1], CHARS[b2], CHARS[b3], b'='])?;
                    }
                    1 => {
                        let b1 = (chunk[0] >> 2) as usize;
                        let b2 = ((chunk[0] & 0x03) << 4) as usize;
                        self.writer
                            .write_all(&[CHARS[b1], CHARS[b2], b'=', b'='])?;
                    }
                    _ => {}
                }
                Ok(())
            }
        }

        impl<W: Write> Write for Encoder<W> {
            #[allow(clippy::indexing_slicing)] // SAFETY: buffer_len is always 0..3, buffer is [u8; 3]
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let mut written = 0;
                for &byte in buf {
                    self.buffer[self.buffer_len] = byte;
                    self.buffer_len += 1;

                    if self.buffer_len == 3 {
                        let buffer_copy = self.buffer;
                        self.encode_chunk(&buffer_copy)?;
                        self.buffer_len = 0;
                    }
                    written += 1;
                }
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                if self.buffer_len > 0 {
                    let buffer_len = self.buffer_len;
                    let buffer_slice: Vec<u8> = self.buffer.get(..buffer_len).unwrap_or_default().to_vec();
                    self.encode_chunk(&buffer_slice)?;
                    self.buffer_len = 0;
                }
                self.writer.flush()
            }
        }

        impl<W: Write> Drop for Encoder<W> {
            fn drop(&mut self) {
                let _ = self.flush();
            }
        }
    }

    mod base64_decode {
        use std::io::{self, Read};

        pub struct Decoder<R: Read> {
            reader: R,
            buffer: Vec<u8>,
            pos: usize,
        }

        impl<R: Read> Decoder<R> {
            pub fn new(reader: R) -> Self {
                Self {
                    reader,
                    buffer: Vec::new(),
                    pos: 0,
                }
            }

            fn char_to_value(c: u8) -> Option<u8> {
                match c {
                    b'A'..=b'Z' => Some(c - b'A'),
                    b'a'..=b'z' => Some(c - b'a' + 26),
                    b'0'..=b'9' => Some(c - b'0' + 52),
                    b'+' => Some(62),
                    b'/' => Some(63),
                    b'=' => Some(64), // Padding
                    _ => None,
                }
            }

            #[allow(clippy::indexing_slicing)] // SAFETY: values length checked before each access (>= 2, 3, 4)
            fn decode_chunk(chunk: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
                if chunk.is_empty() {
                    return Ok(());
                }

                let values: Vec<u8> = chunk
                    .iter()
                    .filter_map(|&c| Self::char_to_value(c))
                    .collect();

                if values.len() < 2 {
                    return Ok(());
                }

                let b1 = (values[0] << 2) | (values[1] >> 4);
                output.push(b1);

                if values.len() >= 3 && values[2] != 64 {
                    let b2 = ((values[1] & 0x0F) << 4) | (values[2] >> 2);
                    output.push(b2);

                    if values.len() >= 4 && values[3] != 64 {
                        let b3 = ((values[2] & 0x03) << 6) | values[3];
                        output.push(b3);
                    }
                }

                Ok(())
            }
        }

        impl<R: Read> Read for Decoder<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.buffer.is_empty() {
                    let mut input = Vec::new();
                    self.reader.read_to_end(&mut input)?;

                    let mut chunk = Vec::new();
                    for &byte in &input {
                        if byte.is_ascii_whitespace() {
                            continue;
                        }
                        chunk.push(byte);
                        if chunk.len() == 4 {
                            Self::decode_chunk(&chunk, &mut self.buffer)?;
                            chunk.clear();
                        }
                    }
                    if !chunk.is_empty() {
                        Self::decode_chunk(&chunk, &mut self.buffer)?;
                    }
                    self.pos = 0;
                }

                let available = self.buffer.len().saturating_sub(self.pos);
                let to_copy = available.min(buf.len());
                if let (Some(dst), Some(src)) = (buf.get_mut(..to_copy), self.buffer.get(self.pos..self.pos + to_copy)) {
                    dst.copy_from_slice(src);
                }
                self.pos += to_copy;

                Ok(to_copy)
            }
        }
    }
}

/// Simple password-based authentication (for testing)
pub struct SimpleAuth {
    username: String,
    password: String,
}

impl SimpleAuth {
    /// Create a new simple authenticator
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }

    /// Verify credentials
    pub fn verify(&self, username: &str, password: &str) -> bool {
        self.username == username && self.password == password
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_auth() {
        let auth = SimpleAuth::new("user".to_string(), "pass".to_string());
        assert!(auth.verify("user", "pass"));
        assert!(!auth.verify("user", "wrong"));
        assert!(!auth.verify("wrong", "pass"));
    }

    #[test]
    fn test_scram_key_derivation() {
        let password = "pencil";
        let salt = b"QSXCR+Q6sek8bf92";
        let (stored_key, _server_key) = ScramAuth::derive_keys(password, salt, 4096);
        assert!(!stored_key.is_empty());
    }

    #[test]
    fn test_base64_encode_decode() {
        let data = b"Hello, World!";
        let encoded = base64::encode(data);
        let decoded = base64::decode(&encoded).unwrap();
        assert_eq!(data, decoded.as_slice());
    }
}
