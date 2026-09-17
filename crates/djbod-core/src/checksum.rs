//! Block checksums, SPEC 8.3.
//!
//! Every shard block carries a 64-bit checksum of its stored, padded bytes
//! (8.3.1). The algorithm is XXH3-64 and is fixed by on-disk format
//! version 1 (8.3.2); it is not configurable. A mismatch turns the block
//! into an erasure for the decoder (8.3.4). The parity shards are never
//! used to detect corruption (8.3.5).

use xxhash_rust::xxh3::xxh3_64;

/// The checksum of one shard block.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockChecksum(pub u64);

impl std::fmt::Debug for BlockChecksum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlockChecksum({:016x})", self.0)
    }
}

/// Compute the checksum of a shard block.
pub fn checksum_block(block: &[u8]) -> BlockChecksum {
    BlockChecksum(xxh3_64(block))
}

/// True if the block's checksum equals the one stored for it.
pub fn block_matches_checksum(block: &[u8], stored: BlockChecksum) -> bool {
    checksum_block(block) == stored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_block_has_the_published_xxh3_value() {
        // Reference value from the xxHash specification for XXH3_64bits of
        // zero bytes with the default seed. Pins the algorithm.
        assert_eq!(checksum_block(&[]), BlockChecksum(0x2D06_8005_38D3_94C2));
    }

    #[test]
    fn checksum_is_deterministic() {
        let block = [7u8; 4096];
        assert_eq!(checksum_block(&block), checksum_block(&block));
    }

    #[test]
    fn single_bit_flip_changes_checksum() {
        let mut block = vec![0x5Au8; 1 << 20];
        let original = checksum_block(&block);
        block[123_456] ^= 0x01;
        assert_ne!(checksum_block(&block), original);
        assert!(!block_matches_checksum(&block, original));
        block[123_456] ^= 0x01;
        assert!(block_matches_checksum(&block, original));
    }

    #[test]
    fn length_is_part_of_the_checksum() {
        assert_ne!(checksum_block(&[0u8; 1]), checksum_block(&[0u8; 2]));
    }
}
