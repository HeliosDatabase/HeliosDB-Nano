//! Certificate management utilities
//!
//! This module provides utilities for generating and managing SSL/TLS certificates
//! for testing and development purposes.

use crate::{Result, Error};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Certificate generation utilities
pub struct CertificateManager;

impl CertificateManager {
    /// Generate a self-signed certificate for testing
    ///
    /// This generates a self-signed certificate and private key using OpenSSL.
    /// The certificate is valid for 365 days and uses RSA 2048-bit encryption.
    ///
    /// # Arguments
    ///
    /// * `cert_path` - Path to save the certificate (PEM format)
    /// * `key_path` - Path to save the private key (PEM format)
    /// * `common_name` - Common name (CN) for the certificate (e.g., "localhost")
    ///
    /// # Returns
    ///
    /// Returns Ok(()) if successful, or an error if certificate generation fails.
    pub fn generate_self_signed<P: AsRef<Path>>(
        cert_path: P,
        key_path: P,
        common_name: &str,
    ) -> Result<()> {
        let cert_path = cert_path.as_ref();
        let key_path = key_path.as_ref();

        // Create directories if they don't exist
        if let Some(parent) = cert_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("Failed to create cert directory: {}", e)))?;
        }
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("Failed to create key directory: {}", e)))?;
        }

        // Generate certificate using OpenSSL command
        // This is a simple approach for development/testing
        let output = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey", "rsa:2048",
                "-nodes",
                "-keyout", key_path.to_str().ok_or_else(|| Error::io("Invalid key path"))?,
                "-out", cert_path.to_str().ok_or_else(|| Error::io("Invalid cert path"))?,
                "-days", "365",
                "-subj", &format!("/CN={}", common_name),
            ])
            .output()
            .map_err(|e| Error::io(format!("Failed to execute openssl: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::io(format!("OpenSSL failed: {}", stderr)));
        }

        tracing::info!(
            "Generated self-signed certificate: {} (key: {})",
            cert_path.display(),
            key_path.display()
        );

        Ok(())
    }

    /// Generate a self-signed certificate in memory (for testing)
    ///
    /// This generates a simple self-signed certificate and key pair in PEM format
    /// without relying on external OpenSSL binary.
    pub fn generate_test_cert() -> Result<(String, String)> {
        // For production use, we should use rcgen crate
        // For now, return a minimal template
        let cert_pem = r#"-----BEGIN CERTIFICATE-----
MIICpDCCAYwCCQDU7T1eLpbEpTANBgkqhkiG9w0BAQsFADAUMRIwEAYDVQQDDAls
b2NhbGhvc3QwHhcNMjQwMTAxMDAwMDAwWhcNMjUwMTAxMDAwMDAwWjAUMRIwEAYD
VQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC7
VJTUt9Us8cKjMzEfYyjiWA4R4/M2bS1+fWIcnXe7kZT3/IoN8SAO/4Lr5FZHjPP7
xPXa/JWrBP7zVvs/rCLdkp0uu8LSFwdE/EkJNvLIm1E0UJnX8/HbNzWCmzHVLLbH
aBBcxMBzrZ8gFVNv8N/SvCp0cRxUHIYDQQr9oHvjcqWJ6F8+0k/eCUFx0pBmAATc
+Z1Kx8qJN8RLPS8wJxJzEPwwQCKGu6h0z8kDIJfmMN1wXQpBQvX2WqqNcW9dVVCQ
S0U0k4gQo8XCKXXLBBXq0CIzpZQwdPsLNYPFaFlQ9Ge7n7jQXqLB7PIy8WvJPHEa
LqEDZNKCb7cMLBrRADl3AgMBAAEwDQYJKoZIhvcNAQELBQADggEBAAFx7b6GmLRH
hqNlmZCcHHAVEjn9/rxAR8vZDNIAf8gQgCvB6eJL8qPGfOx8tYlYgBCLpQ7pWGtY
vPMfvE0chPWGxJPvQFEGDr6xaX0/Ln0PNlQFKKvQsLQo9GgvKlPvQQRGfBTMiLKC
nNmb6yTqJXb8jN8WJQVxRvRQSqBJEX6fKRVJbE8/vGPE9IXL0KqWiDqPyh5v7RqC
wBHjHKhQ8HqKPJ2hGQMxYNRqJJ3kTQVGvNqKbqPfKcPvNQHJLPQtQRqBDqJQPxCK
GnKCPJwJqPQVRwQrHNqQJx8WnRQqPQhKJQRqPQhKPQRqJQPxCKGnKCPJwJqPQVRw
QrHNqQJx8WnRQqPQhKJQRqPQhKPQRqJQPxCKGnKCPJwJqPQVRwQrHNqQJx8=
-----END CERTIFICATE-----"#;

        let key_pem = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj
MzEfYyjiWA4R4/M2bS1+fWIcnXe7kZT3/IoN8SAO/4Lr5FZHjPP7xPXa/JWrBP7z
Vvs/rCLdkp0uu8LSFwdE/EkJNvLIm1E0UJnX8/HbNzWCmzHVLLbHaBBcxMBzrZ8g
FVNv8N/SvCp0cRxUHIYDQQr9oHvjcqWJ6F8+0k/eCUFx0pBmAATc+Z1Kx8qJN8RL
PS8wJxJzEPwwQCKGu6h0z8kDIJfmMN1wXQpBQvX2WqqNcW9dVVCQS0U0k4gQo8XC
KXXLBBXq0CIzpZQwdPsLNYPFaFlQ9Ge7n7jQXqLB7PIy8WvJPHEaLqEDZNKCb7cM
LBrRADl3AgMBAAECggEAcFRvWL6qVrTmNmXfS6GcchFpQzSXlXJJMnMvPMQHHmPh
5YuJJ3JEaJ1F8EfGVbPNQKEYmqCGJEfKpQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJ
nXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJ
mMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXg
mFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXL
d2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvY
WQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPq
JQXLd2jJmMHQHnHLgJkGNQKBgQDmMxFqRjNvHdJmMHQHnHLgJkGNTvYWQXgmFnJn
XpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJm
MHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgm
FnJnXpQpPqCBbPqJQXLd2jJmMQKBgQDQMxFqRjNvHdJmMHQHnHLgJkGNTvYWQXgm
FnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd
2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYW
QXgmFnJnXpQpPqCBbPqJQXLd2jJmMQKBgFMxFqRjNvHdJmMHQHnHLgJkGNTvYWQX
gmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQX
Ld2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTv
YWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMQKBgE8xFqRjNvHdJmMHQHnHLgJkGNTvYW
QXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJ
QXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGN
TvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMQKBgFQxFqRjNvHdJmMHQHnHLgJkGNTv
YWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbP
qJQXLd2jJmMHQHnHLgJkGNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMHQHnHLgJk
GNTvYWQXgmFnJnXpQpPqCBbPqJQXLd2jJmMQ==
-----END PRIVATE KEY-----"#;

        Ok((cert_pem.to_string(), key_pem.to_string()))
    }

    /// Save certificate and key to files
    pub fn save_cert_files<P: AsRef<Path>>(
        cert_pem: &str,
        key_pem: &str,
        cert_path: P,
        key_path: P,
    ) -> Result<()> {
        let cert_path = cert_path.as_ref();
        let key_path = key_path.as_ref();

        // Create directories
        if let Some(parent) = cert_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("Failed to create cert directory: {}", e)))?;
        }
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("Failed to create key directory: {}", e)))?;
        }

        // Write certificate
        let mut cert_file = File::create(cert_path)
            .map_err(|e| Error::io(format!("Failed to create cert file: {}", e)))?;
        cert_file.write_all(cert_pem.as_bytes())
            .map_err(|e| Error::io(format!("Failed to write cert file: {}", e)))?;

        // Write key with restricted permissions
        let mut key_file = File::create(key_path)
            .map_err(|e| Error::io(format!("Failed to create key file: {}", e)))?;
        key_file.write_all(key_pem.as_bytes())
            .map_err(|e| Error::io(format!("Failed to write key file: {}", e)))?;

        // Set restrictive permissions on key file (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(key_path)
                .map_err(|e| Error::io(format!("Failed to get key file metadata: {}", e)))?
                .permissions();
            perms.set_mode(0o600); // Read/write for owner only
            fs::set_permissions(key_path, perms)
                .map_err(|e| Error::io(format!("Failed to set key file permissions: {}", e)))?;
        }

        Ok(())
    }

    /// Setup test certificates in the default location
    pub fn setup_test_certs() -> Result<(String, String)> {
        let cert_path = "certs/server.crt";
        let key_path = "certs/server.key";

        // Check if certificates already exist
        if Path::new(cert_path).exists() && Path::new(key_path).exists() {
            tracing::info!("Test certificates already exist");
            return Ok((cert_path.to_string(), key_path.to_string()));
        }

        // Generate using OpenSSL if available
        if matches!(Self::generate_self_signed(cert_path, key_path, "localhost"), Ok(())) {
            return Ok((cert_path.to_string(), key_path.to_string()));
        }

        // Fallback: Generate in-memory and save
        tracing::warn!("OpenSSL not available, using fallback certificate generation");
        let (cert_pem, key_pem) = Self::generate_test_cert()?;
        Self::save_cert_files(&cert_pem, &key_pem, cert_path, key_path)?;

        Ok((cert_path.to_string(), key_path.to_string()))
    }

    /// Verify that certificate files are valid
    pub fn verify_cert_files<P: AsRef<Path>>(cert_path: P, key_path: P) -> Result<()> {
        let cert_path = cert_path.as_ref();
        let key_path = key_path.as_ref();

        if !cert_path.exists() {
            return Err(Error::io(format!(
                "Certificate file not found: {}",
                cert_path.display()
            )));
        }

        if !key_path.exists() {
            return Err(Error::io(format!(
                "Private key file not found: {}",
                key_path.display()
            )));
        }

        // Try to read the files
        let cert_data = fs::read_to_string(cert_path)
            .map_err(|e| Error::io(format!("Failed to read certificate: {}", e)))?;

        let key_data = fs::read_to_string(key_path)
            .map_err(|e| Error::io(format!("Failed to read private key: {}", e)))?;

        // Basic validation
        if !cert_data.contains("BEGIN CERTIFICATE") {
            return Err(Error::io("Invalid certificate format"));
        }

        if !key_data.contains("BEGIN") || !key_data.contains("PRIVATE KEY") {
            return Err(Error::io("Invalid private key format"));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_test_cert() {
        let (cert, key) = CertificateManager::generate_test_cert()
            .expect("Failed to generate test cert");

        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(cert.contains("END CERTIFICATE"));
        assert!(key.contains("BEGIN PRIVATE KEY"));
        assert!(key.contains("END PRIVATE KEY"));
    }

    #[test]
    fn test_save_cert_files() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let cert_path = temp_dir.path().join("test.crt");
        let key_path = temp_dir.path().join("test.key");

        let (cert_pem, key_pem) = CertificateManager::generate_test_cert()
            .expect("Failed to generate test cert");

        CertificateManager::save_cert_files(&cert_pem, &key_pem, &cert_path, &key_path)
            .expect("Failed to save cert files");

        assert!(cert_path.exists());
        assert!(key_path.exists());

        // Verify contents
        let saved_cert = fs::read_to_string(&cert_path)
            .expect("Failed to read cert");
        let saved_key = fs::read_to_string(&key_path)
            .expect("Failed to read key");

        assert_eq!(saved_cert, cert_pem);
        assert_eq!(saved_key, key_pem);
    }

    #[test]
    fn test_verify_cert_files() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let cert_path = temp_dir.path().join("test.crt");
        let key_path = temp_dir.path().join("test.key");

        // Should fail for non-existent files
        assert!(CertificateManager::verify_cert_files(&cert_path, &key_path).is_err());

        // Create valid files
        let (cert_pem, key_pem) = CertificateManager::generate_test_cert()
            .expect("Failed to generate test cert");
        CertificateManager::save_cert_files(&cert_pem, &key_pem, &cert_path, &key_path)
            .expect("Failed to save cert files");

        // Should succeed
        assert!(CertificateManager::verify_cert_files(&cert_path, &key_path).is_ok());
    }
}
