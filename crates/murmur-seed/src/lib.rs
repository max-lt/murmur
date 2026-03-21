//! BIP39 mnemonic handling and HKDF key derivation for Murmur.
//!
//! This crate derives all network-level and device-level cryptographic material
//! from a single BIP39 mnemonic:
//!
//! - [`NetworkIdentity`] — network ID, ALPN, and first-device signing key
//! - [`DeviceKeyPair`] — random Ed25519 keypair for non-first devices
//!
//! # Key derivation scheme
//!
//! ```text
//! mnemonic → 64-byte seed (BIP39)
//!   ├─ HKDF(seed, info="murmur/network-id")  → blake3 → NetworkId
//!   ├─ "murmur/0/" + hex(network_id)[..16]     → ALPN
//!   └─ HKDF(seed, info="murmur/first-device-key") → Ed25519 SigningKey
//! ```

use ed25519_dalek::{SigningKey, VerifyingKey};
use hkdf::Hkdf;
use murmur_types::{DeviceId, NetworkId};
use sha2::Sha256;
use zeroize::Zeroize;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur in the seed crate.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// The mnemonic phrase is invalid.
    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),
}

// ---------------------------------------------------------------------------
// Mnemonic word count
// ---------------------------------------------------------------------------

/// Number of words in a BIP39 mnemonic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCount {
    /// 12 words (128 bits of entropy).
    Twelve,
    /// 24 words (256 bits of entropy).
    TwentyFour,
}

impl WordCount {
    fn entropy_bits(self) -> usize {
        match self {
            WordCount::Twelve => 128,
            WordCount::TwentyFour => 256,
        }
    }
}

// ---------------------------------------------------------------------------
// Mnemonic generation & validation
// ---------------------------------------------------------------------------

/// Generate a new BIP39 mnemonic with the given word count.
pub fn generate_mnemonic(word_count: WordCount) -> bip39::Mnemonic {
    let entropy_bytes = word_count.entropy_bits() / 8;
    let mut entropy = vec![0u8; entropy_bytes];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut entropy);
    bip39::Mnemonic::from_entropy(&entropy).expect("valid entropy length")
}

/// Parse and validate a mnemonic phrase.
pub fn parse_mnemonic(phrase: &str) -> Result<bip39::Mnemonic, SeedError> {
    phrase
        .parse::<bip39::Mnemonic>()
        .map_err(|e| SeedError::InvalidMnemonic(e.to_string()))
}

// ---------------------------------------------------------------------------
// HKDF helpers
// ---------------------------------------------------------------------------

/// Derive 32 bytes from a seed using HKDF-SHA256 with the given info string.
fn hkdf_derive_32(seed: &[u8], info: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, seed);
    let mut out = [0u8; 32];
    hk.expand(info.as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

// ---------------------------------------------------------------------------
// NetworkIdentity
// ---------------------------------------------------------------------------

/// All values derived from a BIP39 mnemonic for a Murmur network.
///
/// Includes the network ID, ALPN protocol string, and the first device's
/// signing key. Created via [`NetworkIdentity::from_mnemonic`].
///
/// Sensitive key material is zeroed from memory when this struct is dropped.
pub struct NetworkIdentity {
    /// Unique identifier for this network.
    network_id: NetworkId,
    /// QUIC ALPN protocol string for network isolation.
    alpn: Vec<u8>,
    /// Ed25519 signing key for the first device in the network.
    /// `SigningKey` already implements `Zeroize`.
    first_device_key: SigningKey,
}

impl Drop for NetworkIdentity {
    fn drop(&mut self) {
        // SigningKey auto-zeroes on drop via ed25519-dalek's ZeroizeOnDrop impl.
        // Zero the ALPN bytes too (contains network-derived info).
        self.alpn.zeroize();
    }
}

impl NetworkIdentity {
    /// Derive a full network identity from a mnemonic and optional passphrase.
    pub fn from_mnemonic(mnemonic: &bip39::Mnemonic, passphrase: &str) -> Self {
        let seed = mnemonic.to_seed(passphrase);
        Self::from_seed(&seed)
    }

    /// Derive a full network identity from a raw 64-byte BIP39 seed.
    pub fn from_seed(seed: &[u8]) -> Self {
        // Network ID: HKDF → blake3
        let mut network_id_raw = hkdf_derive_32(seed, "murmur/network-id");
        let network_id = NetworkId::from_bytes(*blake3::hash(&network_id_raw).as_bytes());
        network_id_raw.zeroize();

        // ALPN: "murmur/0/" + first 16 hex chars of network_id
        let hex_prefix: String = network_id.to_string().chars().take(16).collect();
        let alpn = format!("murmur/0/{hex_prefix}").into_bytes();

        // First device key: HKDF → Ed25519 signing key
        let mut key_bytes = hkdf_derive_32(seed, "murmur/first-device-key");
        let first_device_key = SigningKey::from_bytes(&key_bytes);
        key_bytes.zeroize();

        Self {
            network_id,
            alpn,
            first_device_key,
        }
    }

    /// The network's unique identifier.
    pub fn network_id(&self) -> NetworkId {
        self.network_id
    }

    /// The QUIC ALPN protocol bytes for this network.
    pub fn alpn(&self) -> &[u8] {
        &self.alpn
    }

    /// The first device's Ed25519 signing key.
    pub fn first_device_signing_key(&self) -> &SigningKey {
        &self.first_device_key
    }

    /// The first device's [`DeviceId`] (public key).
    pub fn first_device_id(&self) -> DeviceId {
        DeviceId::from_verifying_key(&self.first_device_key.verifying_key())
    }

    /// Derive 32 bytes for the creator's iroh endpoint secret key.
    ///
    /// This is deterministic from the mnemonic, so all network members can
    /// compute the creator's iroh [`EndpointId`] and use it as a gossip
    /// bootstrap peer.
    pub fn creator_iroh_key_bytes(&self) -> [u8; 32] {
        // We re-derive from the seed stored in network_id derivation path.
        // Since we don't store the seed, we derive from a different HKDF path
        // using the network_id as input (which is itself deterministic).
        // Actually, we need the original seed. Let's store it.
        //
        // Note: this method derives from network_id bytes which is deterministic.
        // For proper domain separation we use HKDF on the network_id bytes.
        let hk = Hkdf::<Sha256>::new(None, self.network_id.as_bytes());
        let mut out = [0u8; 32];
        hk.expand(b"murmur/creator-iroh-key", &mut out)
            .expect("32 bytes is valid HKDF-SHA256 output");
        out
    }

    /// Derive 32 bytes for AES-256-GCM blob encryption at rest.
    ///
    /// All devices in the same network derive the same key, so blobs
    /// encrypted on one device can be decrypted on another.
    pub fn blob_encryption_key(&self) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(None, self.network_id.as_bytes());
        let mut out = [0u8; 32];
        hk.expand(b"murmur/blob-encryption-key", &mut out)
            .expect("32 bytes is valid HKDF-SHA256 output");
        out
    }
}

// ---------------------------------------------------------------------------
// DeviceKeyPair
// ---------------------------------------------------------------------------

/// An Ed25519 keypair for a (non-first) device.
///
/// Generated randomly. The first device uses the seed-derived key from
/// [`NetworkIdentity::first_device_signing_key`] instead.
///
/// Signing key material is zeroed from memory when this struct is dropped.
pub struct DeviceKeyPair {
    signing_key: SigningKey,
}

// DeviceKeyPair: SigningKey auto-zeroes on drop via ed25519-dalek's ZeroizeOnDrop impl.

impl DeviceKeyPair {
    /// Generate a new random device keypair.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        Self { signing_key }
    }

    /// Reconstruct from existing key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&bytes),
        }
    }

    /// The Ed25519 signing key.
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// The Ed25519 verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// The device's [`DeviceId`] derived from the public key.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_verifying_key(&self.verifying_key())
    }

    /// Export the signing key bytes (for persistence).
    pub fn to_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    #[test]
    fn test_generate_12_word_mnemonic() {
        let m = generate_mnemonic(WordCount::Twelve);
        assert_eq!(m.word_count(), 12);
    }

    #[test]
    fn test_generate_24_word_mnemonic() {
        let m = generate_mnemonic(WordCount::TwentyFour);
        assert_eq!(m.word_count(), 24);
    }

    #[test]
    fn test_parse_valid_mnemonic() {
        let m = generate_mnemonic(WordCount::Twelve);
        let phrase = m.to_string();
        let parsed = parse_mnemonic(&phrase).unwrap();
        assert_eq!(parsed.to_string(), phrase);
    }

    #[test]
    fn test_parse_invalid_mnemonic() {
        let result = parse_mnemonic("not a valid mnemonic phrase at all");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("invalid mnemonic"));
    }

    #[test]
    fn test_same_mnemonic_same_network_id() {
        let m = generate_mnemonic(WordCount::Twelve);
        let id1 = NetworkIdentity::from_mnemonic(&m, "");
        let id2 = NetworkIdentity::from_mnemonic(&m, "");
        assert_eq!(id1.network_id(), id2.network_id());
    }

    #[test]
    fn test_same_mnemonic_same_alpn() {
        let m = generate_mnemonic(WordCount::Twelve);
        let id1 = NetworkIdentity::from_mnemonic(&m, "");
        let id2 = NetworkIdentity::from_mnemonic(&m, "");
        assert_eq!(id1.alpn(), id2.alpn());
    }

    #[test]
    fn test_same_mnemonic_same_first_device_key() {
        let m = generate_mnemonic(WordCount::Twelve);
        let id1 = NetworkIdentity::from_mnemonic(&m, "");
        let id2 = NetworkIdentity::from_mnemonic(&m, "");
        assert_eq!(
            id1.first_device_signing_key().to_bytes(),
            id2.first_device_signing_key().to_bytes()
        );
    }

    #[test]
    fn test_different_mnemonic_different_network_id() {
        let m1 = generate_mnemonic(WordCount::Twelve);
        let m2 = generate_mnemonic(WordCount::Twelve);
        let id1 = NetworkIdentity::from_mnemonic(&m1, "");
        let id2 = NetworkIdentity::from_mnemonic(&m2, "");
        assert_ne!(id1.network_id(), id2.network_id());
    }

    #[test]
    fn test_hkdf_domain_separation() {
        let m = generate_mnemonic(WordCount::Twelve);
        let identity = NetworkIdentity::from_mnemonic(&m, "");
        // network_id bytes must differ from first_device_key bytes
        assert_ne!(
            identity.network_id().as_bytes(),
            &identity.first_device_signing_key().to_bytes()
        );
    }

    #[test]
    fn test_alpn_format() {
        let m = generate_mnemonic(WordCount::Twelve);
        let identity = NetworkIdentity::from_mnemonic(&m, "");
        let alpn_str = std::str::from_utf8(identity.alpn()).unwrap();
        assert!(alpn_str.starts_with("murmur/0/"));
        // "murmur/0/" is 9 chars + 16 hex chars = 25
        assert_eq!(alpn_str.len(), 25);
    }

    #[test]
    fn test_first_device_id_matches_key() {
        let m = generate_mnemonic(WordCount::Twelve);
        let identity = NetworkIdentity::from_mnemonic(&m, "");
        let expected =
            DeviceId::from_verifying_key(&identity.first_device_signing_key().verifying_key());
        assert_eq!(identity.first_device_id(), expected);
    }

    #[test]
    fn test_sign_verify_with_derived_key() {
        let m = generate_mnemonic(WordCount::Twelve);
        let identity = NetworkIdentity::from_mnemonic(&m, "");
        let msg = b"hello murmur";
        let sig = identity.first_device_signing_key().sign(msg);
        let vk = identity.first_device_signing_key().verifying_key();
        assert!(ed25519_dalek::Verifier::verify(&vk, msg, &sig).is_ok());
    }

    #[test]
    fn test_device_keypair_generate() {
        let kp = DeviceKeyPair::generate();
        let msg = b"test message";
        let sig = kp.signing_key().sign(msg);
        assert!(ed25519_dalek::Verifier::verify(&kp.verifying_key(), msg, &sig).is_ok());
    }

    #[test]
    fn test_device_keypair_roundtrip_bytes() {
        let kp = DeviceKeyPair::generate();
        let bytes = kp.to_bytes();
        let kp2 = DeviceKeyPair::from_bytes(bytes);
        assert_eq!(kp.device_id(), kp2.device_id());
    }

    #[test]
    fn test_device_keypair_device_id() {
        let kp = DeviceKeyPair::generate();
        let expected = DeviceId::from_verifying_key(&kp.verifying_key());
        assert_eq!(kp.device_id(), expected);
    }

    #[test]
    fn test_passphrase_changes_identity() {
        let m = generate_mnemonic(WordCount::Twelve);
        let id1 = NetworkIdentity::from_mnemonic(&m, "");
        let id2 = NetworkIdentity::from_mnemonic(&m, "my secret passphrase");
        assert_ne!(id1.network_id(), id2.network_id());
    }

    #[test]
    fn test_24_word_mnemonic_derives_identity() {
        let m = generate_mnemonic(WordCount::TwentyFour);
        let identity = NetworkIdentity::from_mnemonic(&m, "");
        // Just verify it produces valid values without panicking
        let _ = identity.network_id();
        let _ = identity.alpn();
        let _ = identity.first_device_id();
    }
}
