//! The key hash, SPEC 9.1.
//!
//! Keys are not filesystem-safe, so an object's directory is named by the
//! SHA-256 of the key's raw bytes, untruncated, as 64 lowercase hex
//! characters (9.1.2). No normalisation is applied: two keys that differ in
//! any byte are different keys. The algorithm is fixed by on-disk format
//! version 1.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// The SHA-256 of a key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyHash(pub [u8; 32]);

impl std::fmt::Debug for KeyHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyHash({})", self.to_hex())
    }
}

/// Hash a key.
pub fn hash_key(key: &[u8]) -> KeyHash {
    let digest = Sha256::digest(key);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    KeyHash(bytes)
}

impl KeyHash {
    /// The 64 lowercase hex characters that name the object directory.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Parse the 64 hex characters produced by [`KeyHash::to_hex`].
    pub fn from_hex(hex: &str) -> Option<KeyHash> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            bytes[i] = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(KeyHash(bytes))
    }

    /// The three path components under a bucket directory (9.1.3):
    /// the first two hex characters, the next two, and the full hash.
    pub fn directory_components(&self) -> [String; 3] {
        let hex = self.to_hex();
        [hex[0..2].to_string(), hex[2..4].to_string(), hex]
    }
}

/// In JSON a key hash is its 64 hex characters (9.1.2).
impl Serialize for KeyHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for KeyHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<KeyHash, D::Error> {
        let hex = String::deserialize(deserializer)?;
        KeyHash::from_hex(&hex)
            .ok_or_else(|| D::Error::custom(format!("not a 64-character hex key hash: {hex:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let hash = hash_key(b"round trip");
        assert_eq!(KeyHash::from_hex(&hash.to_hex()), Some(hash));
        assert_eq!(KeyHash::from_hex("abc"), None);
        assert_eq!(KeyHash::from_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn matches_published_sha256_vectors() {
        assert_eq!(
            hash_key(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_key(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn keys_are_raw_bytes_with_no_normalisation() {
        assert_ne!(hash_key(b"a"), hash_key(b"A"));
        assert_ne!(hash_key(b"a"), hash_key(b"a/"));
        // NFC and NFD forms of the same character hash differently.
        assert_ne!(hash_key("é".as_bytes()), hash_key("e\u{0301}".as_bytes()));
    }

    #[test]
    fn directory_components_are_fan_out_then_full_hash() {
        let [first, second, full] = hash_key(b"abc").directory_components();
        assert_eq!(first, "ba");
        assert_eq!(second, "78");
        assert_eq!(full.len(), 64);
        assert!(full.starts_with("ba78"));
    }
}
