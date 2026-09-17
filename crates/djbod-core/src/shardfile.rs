//! The shard file, SPEC 9.3: one device's blocks for one version of one
//! object, on disk.
//!
//! ```text
//! HEADER   4096 bytes at offset 0, written first: identity of the file
//! BLOCKS   block i at 4096 + i × B; the last block may be shorter
//! FOOTER   at offset F, written last: lengths and the checksum table
//! TRAILER  the last 16 bytes: footer length L, footer offset F
//!          invariant: F + L + 16 == file length
//! ```
//!
//! The header is aligned so blocks start on 4096-byte boundaries, and is
//! written at creation so a half-written temporary file identifies itself.
//! The footer holds everything that depends on the object's length, so the
//! format never needs the length up front. Field layout is in SPEC 9.3.2.
//!
//! This module does not choose file names, temporary names, or perform the
//! rename into place (9.3.3); the device layer does. It reads and writes
//! one file at a path.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use thiserror::Error;

use crate::checksum::{checksum_block, BlockChecksum};
use crate::erasure::{Scheme, SchemeError, ShardIndex};
use crate::keyhash::KeyHash;
use crate::stripe::ShardBlock;
use crate::version::VersionId;

/// The first eight bytes of every shard file.
pub const MAGIC: [u8; 8] = *b"DJBOD-SF";
pub const FORMAT_VERSION: u32 = 1;
/// Checksum algorithm identifiers as written in the header.
pub const CHECKSUM_ALGORITHM_XXH3_64: u32 = 1;

pub const HEADER_LEN: u64 = 4096;
pub const FOOTER_FIXED_LEN: u64 = 32;
pub const TRAILER_LEN: u64 = 16;

// Header field offsets (SPEC 9.3.2).
const HDR_MAGIC: usize = 0;
const HDR_FORMAT_VERSION: usize = 8;
const HDR_CHECKSUM_ALGORITHM: usize = 12;
const HDR_FLAGS: usize = 16;
const HDR_K: usize = 20;
const HDR_M: usize = 21;
const HDR_SHARD_INDEX: usize = 22;
const HDR_BLOCK_LENGTH: usize = 24;
const HDR_KEY_HASH: usize = 32;
const HDR_VERSION_ID: usize = 64;
const HDR_CHECKSUM: usize = 80;

// Footer field offsets, relative to the footer start.
const FTR_BLOCK_COUNT: usize = 0;
const FTR_LAST_BLOCK_LENGTH: usize = 8;
const FTR_OBJECT_SIZE: usize = 16;
const FTR_CHECKSUM: usize = 24;
const FTR_TABLE: usize = 32;

/// What is known when a shard file is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardFileHeader {
    pub scheme: Scheme,
    pub shard_index: ShardIndex,
    pub block_length: u64,
    pub key_hash: KeyHash,
    pub version_id: VersionId,
}

/// What is known when a shard file is finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardFileFooter {
    pub block_count: u64,
    pub last_block_length: u64,
    pub object_size: u64,
    pub checksums: Vec<BlockChecksum>,
}

#[derive(Debug, Error)]
pub enum ShardFileError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("file is {0} bytes, too short to be a shard file")]
    TooShort(u64),
    #[error("bad magic: not a shard file")]
    BadMagic,
    #[error("unsupported format version {0}")]
    UnsupportedFormatVersion(u32),
    #[error("unsupported checksum algorithm {0}")]
    UnsupportedChecksumAlgorithm(u32),
    #[error("header checksum mismatch")]
    HeaderChecksumMismatch,
    #[error("header describes an invalid scheme: {0}")]
    InvalidScheme(#[from] SchemeError),
    #[error("header shard index {0} is outside the scheme")]
    ShardIndexOutOfRange(ShardIndex),
    #[error(
        "trailer does not describe this file: footer offset {footer_offset} + footer length {footer_length} + 16 != file length {file_length}"
    )]
    TrailerInvariant {
        footer_offset: u64,
        footer_length: u64,
        file_length: u64,
    },
    #[error("footer length {0} is not 32 + 8n")]
    BadFooterLength(u64),
    #[error("footer checksum mismatch")]
    FooterChecksumMismatch,
    #[error("footer is inconsistent: {0}")]
    InconsistentFooter(String),
    #[error("block {index} is out of range; file has {count} blocks")]
    BlockOutOfRange { index: u64, count: u64 },
    #[error("block carries {actual} but this file holds {expected}")]
    WrongShardIndex {
        expected: ShardIndex,
        actual: ShardIndex,
    },
    #[error("block of {actual} bytes; blocks must be 1 .. {max} bytes")]
    BadBlockLength { actual: usize, max: u64 },
    #[error("a block was appended after a short block, which must be last")]
    BlockAfterShortBlock,
    #[error("block checksum supplied as {supplied:?} but bytes checksum to {computed:?}")]
    SuppliedChecksumMismatch {
        supplied: BlockChecksum,
        computed: BlockChecksum,
    },
    #[error("a shard file must hold at least one block")]
    NoBlocks,
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// Checksum of a region with the 8 bytes at `zeroed_at` treated as zero,
/// used for the header and footer checksums that live inside what they
/// cover.
fn checksum_with_field_zeroed(bytes: &[u8], zeroed_at: usize) -> BlockChecksum {
    let mut copy = bytes.to_vec();
    copy[zeroed_at..zeroed_at + 8].fill(0);
    checksum_block(&copy)
}

impl ShardFileHeader {
    /// Serialize to the 4096-byte header page.
    pub fn encode(&self) -> Vec<u8> {
        let mut page = vec![0u8; HEADER_LEN as usize];
        page[HDR_MAGIC..HDR_MAGIC + 8].copy_from_slice(&MAGIC);
        write_u32(&mut page, HDR_FORMAT_VERSION, FORMAT_VERSION);
        write_u32(
            &mut page,
            HDR_CHECKSUM_ALGORITHM,
            CHECKSUM_ALGORITHM_XXH3_64,
        );
        write_u32(&mut page, HDR_FLAGS, 0);
        page[HDR_K] = self.scheme.data_shards() as u8;
        page[HDR_M] = self.scheme.parity_shards() as u8;
        page[HDR_SHARD_INDEX] = self.shard_index.0;
        write_u64(&mut page, HDR_BLOCK_LENGTH, self.block_length);
        page[HDR_KEY_HASH..HDR_KEY_HASH + 32].copy_from_slice(&self.key_hash.0);
        page[HDR_VERSION_ID..HDR_VERSION_ID + 16].copy_from_slice(&self.version_id.0);
        let checksum = checksum_with_field_zeroed(&page, HDR_CHECKSUM);
        write_u64(&mut page, HDR_CHECKSUM, checksum.0);
        page
    }

    /// Parse and verify a 4096-byte header page.
    pub fn decode(page: &[u8]) -> Result<ShardFileHeader, ShardFileError> {
        if page.len() != HEADER_LEN as usize {
            return Err(ShardFileError::TooShort(page.len() as u64));
        }
        if page[HDR_MAGIC..HDR_MAGIC + 8] != MAGIC {
            return Err(ShardFileError::BadMagic);
        }
        let format_version = read_u32(page, HDR_FORMAT_VERSION);
        if format_version != FORMAT_VERSION {
            return Err(ShardFileError::UnsupportedFormatVersion(format_version));
        }
        let checksum_algorithm = read_u32(page, HDR_CHECKSUM_ALGORITHM);
        if checksum_algorithm != CHECKSUM_ALGORITHM_XXH3_64 {
            return Err(ShardFileError::UnsupportedChecksumAlgorithm(
                checksum_algorithm,
            ));
        }
        let stored = BlockChecksum(read_u64(page, HDR_CHECKSUM));
        if checksum_with_field_zeroed(page, HDR_CHECKSUM) != stored {
            return Err(ShardFileError::HeaderChecksumMismatch);
        }

        let scheme = Scheme::new(page[HDR_K], page[HDR_M])?;
        let shard_index = ShardIndex(page[HDR_SHARD_INDEX]);
        if !scheme.contains(shard_index) {
            return Err(ShardFileError::ShardIndexOutOfRange(shard_index));
        }
        let mut key_hash = [0u8; 32];
        key_hash.copy_from_slice(&page[HDR_KEY_HASH..HDR_KEY_HASH + 32]);
        let mut version_id = [0u8; 16];
        version_id.copy_from_slice(&page[HDR_VERSION_ID..HDR_VERSION_ID + 16]);

        Ok(ShardFileHeader {
            scheme,
            shard_index,
            block_length: read_u64(page, HDR_BLOCK_LENGTH),
            key_hash: KeyHash(key_hash),
            version_id: VersionId(version_id),
        })
    }
}

impl ShardFileFooter {
    pub fn encoded_length(&self) -> u64 {
        FOOTER_FIXED_LEN + 8 * self.checksums.len() as u64
    }

    /// Serialize footer and trailer together, as they are written and
    /// checksummed together. `footer_offset` is where the footer will sit.
    pub fn encode_with_trailer(&self, footer_offset: u64) -> Vec<u8> {
        let footer_len = self.encoded_length() as usize;
        let mut bytes = vec![0u8; footer_len + TRAILER_LEN as usize];
        write_u64(&mut bytes, FTR_BLOCK_COUNT, self.block_count);
        write_u64(&mut bytes, FTR_LAST_BLOCK_LENGTH, self.last_block_length);
        write_u64(&mut bytes, FTR_OBJECT_SIZE, self.object_size);
        for (i, checksum) in self.checksums.iter().enumerate() {
            write_u64(&mut bytes, FTR_TABLE + 8 * i, checksum.0);
        }
        write_u64(&mut bytes, footer_len, footer_len as u64);
        write_u64(&mut bytes, footer_len + 8, footer_offset);
        let checksum = checksum_with_field_zeroed(&bytes, FTR_CHECKSUM);
        write_u64(&mut bytes, FTR_CHECKSUM, checksum.0);
        bytes
    }

    /// Parse and verify footer plus trailer bytes read from a file.
    pub fn decode_with_trailer(bytes: &[u8]) -> Result<ShardFileFooter, ShardFileError> {
        let footer_len = bytes.len() as u64 - TRAILER_LEN;
        if footer_len < FOOTER_FIXED_LEN || !(footer_len - FOOTER_FIXED_LEN).is_multiple_of(8) {
            return Err(ShardFileError::BadFooterLength(footer_len));
        }
        let stored = BlockChecksum(read_u64(bytes, FTR_CHECKSUM));
        if checksum_with_field_zeroed(bytes, FTR_CHECKSUM) != stored {
            return Err(ShardFileError::FooterChecksumMismatch);
        }
        let block_count = read_u64(bytes, FTR_BLOCK_COUNT);
        let table_entries = (footer_len - FOOTER_FIXED_LEN) / 8;
        if block_count != table_entries {
            return Err(ShardFileError::InconsistentFooter(format!(
                "block count {block_count} but checksum table has {table_entries} entries"
            )));
        }
        if block_count == 0 {
            return Err(ShardFileError::NoBlocks);
        }
        let mut checksums = Vec::with_capacity(block_count as usize);
        for i in 0..block_count as usize {
            checksums.push(BlockChecksum(read_u64(bytes, FTR_TABLE + 8 * i)));
        }
        Ok(ShardFileFooter {
            block_count,
            last_block_length: read_u64(bytes, FTR_LAST_BLOCK_LENGTH),
            object_size: read_u64(bytes, FTR_OBJECT_SIZE),
            checksums,
        })
    }
}

/// Writes one shard file: header at creation, blocks as they are appended,
/// footer and trailer on finish.
pub struct ShardFileWriter {
    file: File,
    header: ShardFileHeader,
    checksums: Vec<BlockChecksum>,
    last_block_length: u64,
    short_block_written: bool,
}

impl ShardFileWriter {
    /// Create the file at `path` and write the header page. The path
    /// should be a temporary name; the caller renames on success (9.3.3).
    pub fn create(path: &Path, header: ShardFileHeader) -> Result<ShardFileWriter, ShardFileError> {
        if !header.scheme.contains(header.shard_index) {
            return Err(ShardFileError::ShardIndexOutOfRange(header.shard_index));
        }
        let mut file = File::create(path)?;
        file.write_all(&header.encode())?;
        Ok(ShardFileWriter {
            file,
            header,
            checksums: Vec::new(),
            last_block_length: 0,
            short_block_written: false,
        })
    }

    pub fn header(&self) -> &ShardFileHeader {
        &self.header
    }

    pub fn blocks_written(&self) -> u64 {
        self.checksums.len() as u64
    }

    /// Append the next block. Its index must be this file's shard index,
    /// its length must be 1 .. B, and only the final block may be shorter
    /// than B. The checksum is recomputed from the bytes and must agree
    /// with the one supplied (SPEC 17.6).
    pub fn append_block(&mut self, block: &ShardBlock) -> Result<(), ShardFileError> {
        if block.index != self.header.shard_index {
            return Err(ShardFileError::WrongShardIndex {
                expected: self.header.shard_index,
                actual: block.index,
            });
        }
        if self.short_block_written {
            return Err(ShardFileError::BlockAfterShortBlock);
        }
        let len = block.bytes.len();
        if len == 0 || len as u64 > self.header.block_length {
            return Err(ShardFileError::BadBlockLength {
                actual: len,
                max: self.header.block_length,
            });
        }
        let computed = checksum_block(&block.bytes);
        if computed != block.checksum {
            return Err(ShardFileError::SuppliedChecksumMismatch {
                supplied: block.checksum,
                computed,
            });
        }
        self.file.write_all(&block.bytes)?;
        self.checksums.push(computed);
        self.last_block_length = len as u64;
        if (len as u64) < self.header.block_length {
            self.short_block_written = true;
        }
        Ok(())
    }

    /// Write the footer and trailer and flush everything to disk. The file
    /// is complete once this returns.
    pub fn finish(mut self, object_size: u64) -> Result<ShardFileFooter, ShardFileError> {
        if self.checksums.is_empty() {
            return Err(ShardFileError::NoBlocks);
        }
        let footer = ShardFileFooter {
            block_count: self.checksums.len() as u64,
            last_block_length: self.last_block_length,
            object_size,
            checksums: self.checksums,
        };
        let footer_offset = HEADER_LEN
            + (footer.block_count - 1) * self.header.block_length
            + footer.last_block_length;
        self.file
            .write_all(&footer.encode_with_trailer(footer_offset))?;
        self.file.sync_all()?;
        Ok(footer)
    }
}

/// Reads one shard file. Opening verifies the trailer invariant, the
/// footer checksum, the header checksum, and that the three agree on the
/// file's geometry. Blocks are returned with their stored checksums and
/// are not verified here; the stripe decoder does that.
pub struct ShardFileReader {
    file: File,
    header: ShardFileHeader,
    footer: ShardFileFooter,
}

impl ShardFileReader {
    pub fn open(path: &Path) -> Result<ShardFileReader, ShardFileError> {
        let file = File::open(path)?;
        let file_length = file.metadata()?.len();
        if file_length < HEADER_LEN + 1 + FOOTER_FIXED_LEN + 8 + TRAILER_LEN {
            return Err(ShardFileError::TooShort(file_length));
        }

        // Trailer, then the invariant that ties it to the file.
        let mut trailer = [0u8; TRAILER_LEN as usize];
        file.read_exact_at(&mut trailer, file_length - TRAILER_LEN)?;
        let footer_length = read_u64(&trailer, 0);
        let footer_offset = read_u64(&trailer, 8);
        let described_length = footer_offset
            .checked_add(footer_length)
            .and_then(|n| n.checked_add(TRAILER_LEN));
        if described_length != Some(file_length) {
            return Err(ShardFileError::TrailerInvariant {
                footer_offset,
                footer_length,
                file_length,
            });
        }
        if footer_length < FOOTER_FIXED_LEN || !(footer_length - FOOTER_FIXED_LEN).is_multiple_of(8)
        {
            return Err(ShardFileError::BadFooterLength(footer_length));
        }

        // Footer with its trailer, checksummed together.
        let mut footer_bytes = vec![0u8; (footer_length + TRAILER_LEN) as usize];
        file.read_exact_at(&mut footer_bytes, footer_offset)?;
        let footer = ShardFileFooter::decode_with_trailer(&footer_bytes)?;

        // Header.
        let mut page = vec![0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut page, 0)?;
        let header = ShardFileHeader::decode(&page)?;

        // The three parts must agree on where the blocks are.
        if footer.last_block_length == 0 || footer.last_block_length > header.block_length {
            return Err(ShardFileError::InconsistentFooter(format!(
                "last block length {} with block length {}",
                footer.last_block_length, header.block_length
            )));
        }
        let expected_footer_offset =
            HEADER_LEN + (footer.block_count - 1) * header.block_length + footer.last_block_length;
        if expected_footer_offset != footer_offset {
            return Err(ShardFileError::InconsistentFooter(format!(
                "{} blocks of {} bytes plus a last block of {} put the footer at {}, trailer says {}",
                footer.block_count,
                header.block_length,
                footer.last_block_length,
                expected_footer_offset,
                footer_offset
            )));
        }

        Ok(ShardFileReader {
            file,
            header,
            footer,
        })
    }

    pub fn header(&self) -> &ShardFileHeader {
        &self.header
    }

    pub fn footer(&self) -> &ShardFileFooter {
        &self.footer
    }

    pub fn block_count(&self) -> u64 {
        self.footer.block_count
    }

    /// The stored length of block `index`.
    pub fn block_length(&self, index: u64) -> Result<u64, ShardFileError> {
        if index >= self.footer.block_count {
            return Err(ShardFileError::BlockOutOfRange {
                index,
                count: self.footer.block_count,
            });
        }
        if index == self.footer.block_count - 1 {
            Ok(self.footer.last_block_length)
        } else {
            Ok(self.header.block_length)
        }
    }

    /// Read block `index` with the checksum stored for it. Not verified.
    pub fn read_block(&self, index: u64) -> Result<ShardBlock, ShardFileError> {
        let length = self.block_length(index)?;
        let mut bytes = vec![0u8; length as usize];
        self.file
            .read_exact_at(&mut bytes, HEADER_LEN + index * self.header.block_length)?;
        Ok(ShardBlock {
            index: self.header.shard_index,
            bytes,
            checksum: self.footer.checksums[index as usize],
        })
    }
}
