//! Version identifier generation (SPEC 9.2.3).
//!
//! A ULID is 48 bits of Unix milliseconds followed by 80 random bits.
//! Within one millisecond this generator increments the random part
//! rather than drawing afresh, as the ULID specification recommends, so
//! that identifiers from one coordinator sort in creation order even at
//! sub-millisecond spacing. That is what makes replace-on-PUT (9.2.4)
//! well defined for two writes in the same millisecond.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use djbod_core::version::VersionId;

pub struct VersionGenerator {
    last: Mutex<(u64, u128)>,
}

impl Default for VersionGenerator {
    fn default() -> VersionGenerator {
        VersionGenerator::new()
    }
}

impl VersionGenerator {
    pub fn new() -> VersionGenerator {
        VersionGenerator {
            last: Mutex::new((0, 0)),
        }
    }

    pub fn next(&self) -> VersionId {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            & 0x0000_FFFF_FFFF_FFFF;
        let mut last = self.last.lock().expect("generator lock");
        let (last_ms, last_random) = *last;
        let random: u128 = if now_ms > last_ms {
            let bytes: [u8; 10] = rand::random();
            let mut value: u128 = 0;
            for b in bytes {
                value = (value << 8) | b as u128;
            }
            value
        } else {
            // Same millisecond (or a clock that stepped back): increment.
            (last_random + 1) & ((1u128 << 80) - 1)
        };
        let ms = now_ms.max(last_ms);
        *last = (ms, random);
        let value: u128 = ((ms as u128) << 80) | random;
        VersionId(value.to_be_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_strictly_increasing() {
        let generator = VersionGenerator::new();
        let mut previous = generator.next();
        for _ in 0..10_000 {
            let next = generator.next();
            assert!(next > previous, "{next:?} after {previous:?}");
            previous = next;
        }
    }

    #[test]
    fn timestamp_is_in_the_high_bits() {
        let generator = VersionGenerator::new();
        let id = generator.next();
        let value = u128::from_be_bytes(id.0);
        let ms = (value >> 80) as u64;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64;
        assert!(
            now - ms < 10_000,
            "timestamp {ms} is not recent (now {now})"
        );
        // The text form of a current timestamp starts with 01 for decades.
        assert!(id.to_text().starts_with("01"));
    }
}
