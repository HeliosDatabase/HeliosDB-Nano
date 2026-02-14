//! Example: Encryption usage

use heliosdb_nano::crypto::{encrypt, decrypt, derive_key_from_password};
use heliosdb_nano::Result;

fn main() -> Result<()> {
    println!("HeliosDB Lite - Encryption Example\n");

    // Derive encryption key from password
    let password = "my-secure-password";
    let salt = b"random-salt-1234";
    let key = derive_key_from_password(password, salt)?;
    println!("✓ Encryption key derived from password");

    // Encrypt data
    let plaintext = b"Sensitive data to protect";
    let ciphertext = encrypt(&key, plaintext)?;
    println!("✓ Data encrypted ({} bytes → {} bytes)",
        plaintext.len(), ciphertext.len());

    // Decrypt data
    let decrypted = decrypt(&key, &ciphertext)?;
    println!("✓ Data decrypted");

    // Verify
    assert_eq!(plaintext, &decrypted[..]);
    println!("✓ Decryption verified - data matches!");

    println!("\n💡 To use encryption with HeliosDB Lite:");
    println!("   1. Set HELIOSDB_ENCRYPTION_KEY environment variable");
    println!("   2. Or configure in heliosdb.toml:");
    println!("      [encryption]");
    println!("      enabled = true");
    println!("      key_source = {{ environment = \"HELIOSDB_ENCRYPTION_KEY\" }}");

    Ok(())
}
