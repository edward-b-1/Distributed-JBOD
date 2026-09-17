//! Version identifiers, SPEC 9.2.
//!
//! Every body written under a key receives a 128-bit, time-ordered
//! identifier, a ULID (9.2.3), even though v1 keeps at most one version
//! per key. Its text form is the 26-character Crockford base32 encoding
//! defined by the ULID specification, which sorts lexicographically in the
//! same order as the underlying bits, so sorting file names sorts by
//! creation time. Generation belongs to the coordinator; this module
//! defines the value and its text form.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 128-bit version identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VersionId(pub [u8; 16]);

/// Crockford base32: digits and letters without I, L, O, U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const TEXT_LEN: usize = 26;

fn decode_char(c: u8) -> Option<u128> {
    let upper = c.to_ascii_uppercase();
    for (value, letter) in ALPHABET.iter().enumerate() {
        if *letter == upper {
            return Some(value as u128);
        }
    }
    None
}

impl VersionId {
    /// The 26-character ULID text form, uppercase.
    pub fn to_text(&self) -> String {
        let value = u128::from_be_bytes(self.0);
        let mut out = String::with_capacity(TEXT_LEN);
        for i in 0..TEXT_LEN {
            let shift = 5 * (TEXT_LEN - 1 - i);
            let digit = ((value >> shift) & 0x1F) as usize;
            out.push(ALPHABET[digit] as char);
        }
        out
    }

    /// Parse the text form. Case-insensitive. The first character must be
    /// `0` to `7`, since 26 base32 digits hold 130 bits and only 128 are
    /// used.
    pub fn from_text(text: &str) -> Option<VersionId> {
        let bytes = text.as_bytes();
        if bytes.len() != TEXT_LEN {
            return None;
        }
        if !(b'0'..=b'7').contains(&bytes[0]) {
            return None;
        }
        let mut value: u128 = 0;
        for c in bytes {
            value = (value << 5) | decode_char(*c)?;
        }
        Some(VersionId(value.to_be_bytes()))
    }
}

impl std::fmt::Debug for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VersionId({})", self.to_text())
    }
}

impl std::fmt::Display for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl Serialize for VersionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for VersionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<VersionId, D::Error> {
        let text = String::deserialize(deserializer)?;
        VersionId::from_text(&text)
            .ok_or_else(|| D::Error::custom(format!("not a 26-character ULID: {text:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_matches_the_ulid_specification() {
        // Zero and the maximum representable value.
        assert_eq!(VersionId([0u8; 16]).to_text(), "00000000000000000000000000");
        assert_eq!(
            VersionId([0xFFu8; 16]).to_text(),
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
        // A published example: the ULID whose bytes are 0x01 0x8F ... is
        // not standard, so instead check the timestamp component of a known
        // string round-trips.
        let text = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let id = VersionId::from_text(text).expect("failed to parse ULID");
        assert_eq!(id.to_text(), text);
    }

    #[test]
    fn text_round_trips_and_sorts_like_the_bytes() {
        let mut bytes = [0u8; 16];
        let mut ids = Vec::new();
        for i in 0..50u8 {
            bytes[0] = i / 8; // keep the top bits within 128
            bytes[7] = i.wrapping_mul(37);
            bytes[15] = i;
            let id = VersionId(bytes);
            assert_eq!(VersionId::from_text(&id.to_text()), Some(id));
            ids.push(id);
        }
        let mut by_bytes = ids.clone();
        by_bytes.sort();
        let mut by_text = ids.clone();
        by_text.sort_by_key(|id| id.to_text());
        assert_eq!(by_bytes, by_text);
    }

    #[test]
    fn parsing_rejects_bad_input() {
        assert_eq!(VersionId::from_text(""), None);
        assert_eq!(VersionId::from_text("8ZZZZZZZZZZZZZZZZZZZZZZZZZ"), None); // overflow
        assert_eq!(VersionId::from_text("0000000000000000000000000I"), None); // no I
        assert_eq!(VersionId::from_text("0000000000000000000000000"), None); // 25 chars
                                                                             // Lowercase is accepted.
        assert_eq!(
            VersionId::from_text("01arz3ndektsv4rrffq69g5fav"),
            VersionId::from_text("01ARZ3NDEKTSV4RRFFQ69G5FAV")
        );
    }
}
