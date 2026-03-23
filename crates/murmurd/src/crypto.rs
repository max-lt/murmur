//! Password-based encryption for sensitive files (mnemonic, device key).
//!
//! Uses Argon2id for key derivation and AES-256-GCM for encryption.
//! File format: `salt (16 bytes) || nonce (12 bytes) || ciphertext`.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm};
use anyhow::Result;
use argon2::Argon2;

/// Argon2 salt length in bytes.
const SALT_LEN: usize = 16;

/// AES-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;

/// Encrypt plaintext with a password using Argon2id + AES-256-GCM.
///
/// Returns `salt (16) || nonce (12) || ciphertext`.
#[allow(dead_code)] // Used for seed/key encryption (not yet wired into CLI flow).
pub fn encrypt_with_password(plaintext: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    aes_gcm::aead::rand_core::RngCore::fill_bytes(&mut OsRng, &mut salt);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("valid key length");

    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt ciphertext encrypted with [`encrypt_with_password`].
#[allow(dead_code)] // Used for seed/key encryption (not yet wired into CLI flow).
pub fn decrypt_with_password(encrypted: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    if encrypted.len() < SALT_LEN + NONCE_LEN {
        anyhow::bail!("encrypted data too short");
    }

    let (salt, rest) = encrypted.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);

    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("valid key length");
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("decryption failed (wrong password?): {e}"))
}

/// Derive a 32-byte key from password + salt using Argon2id.
fn derive_key(password: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    let argon2 = Argon2::default(); // Argon2id with default params
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password, salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2 key derivation failed: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = b"abandon ability able about above absent absorb abstract";
        let password = b"my-secret-password";

        let encrypted = encrypt_with_password(plaintext, password).unwrap();
        let decrypted = decrypt_with_password(&encrypted, password).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_password_fails() {
        let plaintext = b"sensitive data";
        let encrypted = encrypt_with_password(plaintext, b"correct").unwrap();
        let result = decrypt_with_password(&encrypted, b"wrong");
        assert!(result.is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let plaintext = b"tamper test";
        let mut encrypted = encrypt_with_password(plaintext, b"pass").unwrap();
        // Flip a byte in the ciphertext.
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0xff;
        let result = decrypt_with_password(&encrypted, b"pass");
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_data_differs_from_plaintext() {
        let plaintext = b"not visible in output";
        let encrypted = encrypt_with_password(plaintext, b"pass").unwrap();
        // Encrypted data should be larger (salt + nonce + tag overhead).
        assert!(encrypted.len() > plaintext.len());
        // And should not contain the plaintext.
        assert!(!encrypted.windows(plaintext.len()).any(|w| w == plaintext));
    }

    #[test]
    fn test_short_encrypted_data_rejected() {
        let result = decrypt_with_password(&[0u8; 10], b"pass");
        assert!(result.is_err());
    }
}
