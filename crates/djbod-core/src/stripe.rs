//! Stripe encoding and decoding, SPEC 8.2 and 8.3.
//!
//! A stripe is up to `k × B` bytes of an object. Encoding splits it
//! contiguously into k data blocks (8.2.3), pads a short final stripe
//! (8.2.5), computes m parity blocks (8.2.4), and checksums every block
//! (8.3.1). Decoding verifies every received block against its stored
//! checksum, treats each mismatch, wrong length, or absence as an erasure
//! (8.3.4), and reconstructs the data if at least k blocks are usable.
//!
//! The parity is never consulted to decide whether a block is good
//! (8.3.5). Only the checksum does that.

use crate::checksum::{checksum_block, BlockChecksum};
use crate::erasure::{Coder, CodingError, ShardIndex};
use thiserror::Error;

/// A stripe's blocks and their checksums, in shard index order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedStripe {
    pub blocks: Vec<Vec<u8>>,
    pub checksums: Vec<BlockChecksum>,
    /// The number of object bytes in this stripe before padding.
    pub data_len: usize,
}

impl EncodedStripe {
    pub fn block_len(&self) -> usize {
        self.blocks[0].len()
    }
}

/// Why a received block could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultKind {
    /// No block with this index was received.
    Missing,
    /// The block's length is not the length every block in this stripe
    /// must have.
    WrongLength { expected: usize, actual: usize },
    /// The block's bytes do not match the checksum stored for it.
    ChecksumMismatch {
        stored: BlockChecksum,
        computed: BlockChecksum,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFault {
    pub index: ShardIndex,
    pub kind: FaultKind,
}

/// A block as received from a holder, with the checksum that was stored
/// alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedBlock {
    pub index: ShardIndex,
    pub bytes: Vec<u8>,
    pub stored_checksum: BlockChecksum,
}

/// The object bytes of a decoded stripe, and every block that had to be
/// treated as erased to produce them. An empty `faults` means every data
/// block arrived intact and nothing was reconstructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedStripe {
    pub data: Vec<u8>,
    pub faults: Vec<BlockFault>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StripeError {
    #[error("a stripe must hold at least one byte")]
    Empty,
    #[error(
        "stripe of {actual} bytes exceeds the maximum of {max} for this scheme and block size"
    )]
    TooLarge { actual: usize, max: usize },
    #[error(transparent)]
    Coding(#[from] CodingError),
    #[error(
        "stripe cannot be decoded: {usable} usable blocks, {needed} needed; faults: {faults:?}"
    )]
    Unrecoverable {
        usable: usize,
        needed: usize,
        faults: Vec<BlockFault>,
    },
}

/// The length of every block in a stripe holding `data_len` object
/// bytes: `ceil(data_len / k)` (8.2.5). A full stripe of `k × B` bytes
/// gives `B`.
pub fn block_length_for(coder: &Coder, data_len: usize) -> usize {
    let k = coder.scheme().data_shards();
    data_len.div_ceil(k)
}

/// Encode up to `k × block_size` object bytes into `k + m` checksummed
/// blocks.
pub fn encode_stripe(
    coder: &Coder,
    data: &[u8],
    block_size: usize,
) -> Result<EncodedStripe, StripeError> {
    let scheme = coder.scheme();
    if data.is_empty() {
        return Err(StripeError::Empty);
    }
    let max = scheme.data_shards() * block_size;
    if data.len() > max {
        return Err(StripeError::TooLarge {
            actual: data.len(),
            max,
        });
    }
    let block_len = block_length_for(coder, data.len());

    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(scheme.total_shards());
    for i in 0..scheme.data_shards() {
        let start = i * block_len;
        let end = (start + block_len).min(data.len());
        let mut block = Vec::with_capacity(block_len);
        if start < data.len() {
            block.extend_from_slice(&data[start..end]);
        }
        block.resize(block_len, 0);
        blocks.push(block);
    }
    let parity = coder.compute_parity(&blocks)?;
    blocks.extend(parity);

    let mut checksums = Vec::with_capacity(blocks.len());
    for block in &blocks {
        checksums.push(checksum_block(block));
    }

    Ok(EncodedStripe {
        blocks,
        checksums,
        data_len: data.len(),
    })
}

/// Recover the `data_len` object bytes of a stripe from whatever blocks
/// were received.
///
/// Every block is checked before it is trusted. A block is usable only if
/// it has the right length and its bytes match its stored checksum. Any
/// other block, and any index for which nothing was received, is an
/// erasure. If at least k blocks are usable the data is returned along
/// with the list of erasures; otherwise the error carries that list.
pub fn decode_stripe(
    coder: &Coder,
    received: &[ReceivedBlock],
    data_len: usize,
) -> Result<DecodedStripe, StripeError> {
    let scheme = coder.scheme();
    if data_len == 0 {
        return Err(StripeError::Empty);
    }
    let block_len = block_length_for(coder, data_len);

    // Index the received blocks by shard index, rejecting protocol errors.
    let mut by_index: Vec<Option<&ReceivedBlock>> = vec![None; scheme.total_shards()];
    for block in received {
        if !scheme.contains(block.index) {
            return Err(CodingError::IndexOutOfRange(block.index).into());
        }
        if by_index[block.index.as_usize()].is_some() {
            return Err(CodingError::DuplicateIndex(block.index).into());
        }
        by_index[block.index.as_usize()] = Some(block);
    }

    // Decide, for every shard of the scheme, whether it is usable.
    let mut usable: Vec<(ShardIndex, &[u8])> = Vec::with_capacity(scheme.total_shards());
    let mut faults: Vec<BlockFault> = Vec::new();
    for index in scheme.shard_indices() {
        let Some(block) = by_index[index.as_usize()] else {
            faults.push(BlockFault {
                index,
                kind: FaultKind::Missing,
            });
            continue;
        };
        if block.bytes.len() != block_len {
            faults.push(BlockFault {
                index,
                kind: FaultKind::WrongLength {
                    expected: block_len,
                    actual: block.bytes.len(),
                },
            });
            continue;
        }
        let computed = checksum_block(&block.bytes);
        if computed != block.stored_checksum {
            faults.push(BlockFault {
                index,
                kind: FaultKind::ChecksumMismatch {
                    stored: block.stored_checksum,
                    computed,
                },
            });
            continue;
        }
        usable.push((index, &block.bytes));
    }

    if usable.len() < scheme.data_shards() {
        return Err(StripeError::Unrecoverable {
            usable: usable.len(),
            needed: scheme.data_shards(),
            faults,
        });
    }

    let data_blocks = coder.reconstruct(&usable, &scheme.data_shard_indices())?;

    let mut data = Vec::with_capacity(data_len);
    for block in &data_blocks {
        data.extend_from_slice(block);
    }
    data.truncate(data_len);

    Ok(DecodedStripe { data, faults })
}
