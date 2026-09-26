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
use crate::erasure::{CodingError, ReedSolomonCode, ShardIndex};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One block of one shard: which shard it belongs to, its bytes, and the
/// checksum of those bytes. This is the unit the encoder produces, a
/// device stores, and the decoder verifies. The same thing at three
/// moments, so one type.
///
/// On disk the checksum is not written beside the bytes but in the shard
/// file header's table (SPEC 8.3.3, 9.3). `ShardBlock` is the logical
/// unit; the shard file is one layout of a sequence of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBlock {
    pub index: ShardIndex,
    pub bytes: Vec<u8>,
    pub checksum: BlockChecksum,
}

/// Why a block could not be used, or was never received. Travels with a
/// read's terminating status when the block was reconstructed (SPEC
/// 11.4). The decoder produces the first three; a read that could not
/// open a shard at all reports the rest, for every stripe at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FaultKind {
    /// No block with this index was received, or the shard file is not
    /// there at all. It will not come back on its own; repair rewrites it.
    Missing,
    /// The shard file could not be opened: truncated, a bad header, or
    /// otherwise unreadable. It will not mend itself; repair rewrites it.
    Unreadable { reason: String },
    /// The shard's device is unavailable or its node unreachable (5.6),
    /// which may be temporary. Nothing is known to be wrong with the
    /// shard itself, and repair does nothing about it until the device
    /// has left the document (18.3).
    Unavailable { reason: String },
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

/// The outcome of decoding a stripe. The three cases are distinct variants
/// so that a caller cannot reach the data without naming the case it is
/// in. The fail-stop read path (SPEC 11.4) accepts only `Intact`; a repair
/// job (SPEC 18.4) wants `Repaired`, which carries both the correct data
/// and the list of shards to rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedStripe {
    /// Every requested block arrived with the right length and a matching
    /// checksum. Nothing was reconstructed.
    Intact { data: Vec<u8> },
    /// The data was recovered exactly, but only by treating the listed
    /// blocks as erased and reconstructing around them.
    Repaired {
        data: Vec<u8>,
        faults: Vec<BlockFault>,
    },
    /// Fewer than k blocks were usable. No data. The faults say why.
    Unrecoverable {
        usable: usize,
        needed: usize,
        faults: Vec<BlockFault>,
    },
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
    #[error("{0} was received but not requested")]
    UnrequestedBlock(ShardIndex),
}

/// The length of every block in a stripe holding `data_len` object
/// bytes: `ceil(data_len / k)` (8.2.5). A full stripe of `k × B` bytes
/// gives `B`.
pub fn block_length_for(code: &ReedSolomonCode, data_len: usize) -> usize {
    let k = code.scheme().data_shards();
    data_len.div_ceil(k)
}

/// Encode up to `k × block_size` object bytes into `k + m` shard blocks,
/// returned in shard index order. Every block has the same length,
/// `block_length_for(code, data.len())`.
pub fn encode_stripe(
    code: &ReedSolomonCode,
    data: &[u8],
    block_size: usize,
) -> Result<Vec<ShardBlock>, StripeError> {
    let scheme = code.scheme();
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
    let block_len = block_length_for(code, data.len());

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
    let parity = code.compute_parity(&blocks)?;
    blocks.extend(parity);

    let mut shard_blocks = Vec::with_capacity(blocks.len());
    for (i, bytes) in blocks.into_iter().enumerate() {
        let checksum = checksum_block(&bytes);
        shard_blocks.push(ShardBlock {
            index: ShardIndex(i as u8),
            bytes,
            checksum,
        });
    }
    Ok(shard_blocks)
}

/// Recover the `data_len` object bytes of a stripe from the blocks that
/// were received, out of the blocks that were `requested`.
///
/// Every received block is checked before it is trusted. A block is usable
/// only if it has the right length and its bytes match its stored
/// checksum. Any other received block is a fault, and so is any requested
/// index for which nothing was received. Indices that were not requested
/// are not faults: the read path fetches only the k data blocks (SPEC
/// 11.3) and must not be told the parity is missing. A received block whose
/// index was not requested is a protocol error.
///
/// The `Err` cases are misuse: an empty stripe, or indices that are out
/// of range, duplicated, or not requested. Every outcome of examining the
/// blocks themselves, including failure to recover, is a variant of
/// [`DecodedStripe`].
pub fn decode_stripe(
    code: &ReedSolomonCode,
    requested: &[ShardIndex],
    received: &[ShardBlock],
    data_len: usize,
) -> Result<DecodedStripe, StripeError> {
    let scheme = code.scheme();
    if data_len == 0 {
        return Err(StripeError::Empty);
    }
    let block_len = block_length_for(code, data_len);

    // Note which indices were requested, rejecting protocol errors.
    let mut was_requested: Vec<bool> = vec![false; scheme.total_shards()];
    for index in requested {
        if !scheme.contains(*index) {
            return Err(CodingError::IndexOutOfRange(*index).into());
        }
        if was_requested[index.as_usize()] {
            return Err(CodingError::DuplicateIndex(*index).into());
        }
        was_requested[index.as_usize()] = true;
    }

    // Index the received blocks by shard index, rejecting protocol errors.
    let mut by_index: Vec<Option<&ShardBlock>> = vec![None; scheme.total_shards()];
    for block in received {
        if !scheme.contains(block.index) {
            return Err(CodingError::IndexOutOfRange(block.index).into());
        }
        if !was_requested[block.index.as_usize()] {
            return Err(StripeError::UnrequestedBlock(block.index));
        }
        if by_index[block.index.as_usize()].is_some() {
            return Err(CodingError::DuplicateIndex(block.index).into());
        }
        by_index[block.index.as_usize()] = Some(block);
    }

    // Decide, for every requested shard, whether it is usable.
    let mut usable: Vec<(ShardIndex, &[u8])> = Vec::with_capacity(scheme.total_shards());
    let mut faults: Vec<BlockFault> = Vec::new();
    for index in scheme.shard_indices() {
        if !was_requested[index.as_usize()] {
            continue;
        }
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
        if computed != block.checksum {
            faults.push(BlockFault {
                index,
                kind: FaultKind::ChecksumMismatch {
                    stored: block.checksum,
                    computed,
                },
            });
            continue;
        }
        usable.push((index, &block.bytes));
    }

    if usable.len() < scheme.data_shards() {
        return Ok(DecodedStripe::Unrecoverable {
            usable: usable.len(),
            needed: scheme.data_shards(),
            faults,
        });
    }

    let data_blocks = code.reconstruct(&usable, &scheme.data_shard_indices())?;

    let mut data = Vec::with_capacity(data_len);
    for block in &data_blocks {
        data.extend_from_slice(block);
    }
    data.truncate(data_len);

    if faults.is_empty() {
        Ok(DecodedStripe::Intact { data })
    } else {
        Ok(DecodedStripe::Repaired { data, faults })
    }
}
