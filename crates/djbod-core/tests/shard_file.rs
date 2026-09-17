//! Tests of the shard file format (SPEC 9.3): writing, reading back, the
//! trailer invariant, the two internal checksums, and the end-to-end path
//! from object bytes through stripes to shard files on disk and back.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use djbod_core::checksum::{block_matches_checksum, checksum_block, BlockChecksum};
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::hash_key;
use djbod_core::shardfile::{
    shard_geometry, ShardFileError, ShardFileFooter, ShardFileHeader, ShardFileReader,
    ShardFileWriter, ShardGeometry, FOOTER_FIXED_LEN, HEADER_LEN, MAGIC, TRAILER_LEN,
};
use djbod_core::stripe::{decode_stripe, encode_stripe, DecodedStripe, ShardBlock};
use djbod_core::version::VersionId;

/// Deterministic pseudo-random bytes from an xorshift64* generator.
fn xorshift64_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.push((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8);
    }
    out
}

fn header_for(k: u8, m: u8, shard_index: u8, block_length: u64) -> ShardFileHeader {
    ShardFileHeader {
        scheme: Scheme::new(k, m).expect("failed to construct scheme"),
        shard_index: ShardIndex(shard_index),
        block_length,
        key_hash: hash_key(b"photos/2026/cat.jpg"),
        version_id: VersionId(*b"0123456789abcdef"),
    }
}

/// A block of `len` bytes for shard `index`, with a correct checksum.
fn block_for(index: u8, len: usize, seed: u64) -> ShardBlock {
    let bytes = xorshift64_bytes(len, seed);
    let checksum = checksum_block(&bytes);
    ShardBlock {
        index: ShardIndex(index),
        bytes,
        checksum,
    }
}

/// An object size whose shard geometry is `full_blocks` blocks of B bytes
/// followed by one block of `last_len` bytes: the final stripe holds
/// exactly `last_len × k` bytes so its blocks need no padding.
fn object_size_for(header: &ShardFileHeader, full_blocks: usize, last_len: usize) -> u64 {
    let k = header.scheme.data_shards() as u64;
    (full_blocks as u64) * k * header.block_length + (last_len as u64) * k
}

/// A stand-in whole-object checksum for files whose object bytes the test
/// never assembles.
const FAKE_OBJECT_CHECKSUM: BlockChecksum = BlockChecksum(0x0B1E_C7C4_3C5A_0001);

/// Write a shard file of `full_blocks` blocks of B bytes followed by one
/// block of `last_len` bytes. Returns the blocks written.
fn write_shard_file(
    path: &Path,
    header: ShardFileHeader,
    full_blocks: usize,
    last_len: usize,
) -> Vec<ShardBlock> {
    let block_length = header.block_length as usize;
    let shard_index = header.shard_index.0;
    let object_size = object_size_for(&header, full_blocks, last_len);
    let mut writer = ShardFileWriter::create(path, header).expect("failed to create shard file");
    let mut blocks = Vec::with_capacity(full_blocks + 1);
    for i in 0..full_blocks {
        blocks.push(block_for(shard_index, block_length, i as u64));
    }
    blocks.push(block_for(shard_index, last_len, 99));
    for block in &blocks {
        writer.append_block(block).expect("failed to append block");
    }
    writer
        .finish(object_size, FAKE_OBJECT_CHECKSUM)
        .expect("failed to finish shard file");
    blocks
}

fn flip_byte_at(path: &Path, offset: u64) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("failed to open file for corruption");
    let mut byte = [0u8; 1];
    file.read_exact_at(&mut byte, offset)
        .expect("failed to read byte");
    byte[0] ^= 0x01;
    file.write_all_at(&byte, offset)
        .expect("failed to write byte");
}

fn file_length(path: &Path) -> u64 {
    std::fs::metadata(path).expect("failed to stat file").len()
}

#[test]
fn file_layout_matches_the_specification() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    let block_length = 4096;
    write_shard_file(&path, header_for(3, 1, 2, block_length), 3, 1000);

    // 4 KiB header, three full blocks, a 1000-byte block, footer of
    // 32 + 8 × 4, trailer of 16.
    let n = 4u64;
    let expected_footer_offset = HEADER_LEN + 3 * block_length + 1000;
    let expected_footer_length = FOOTER_FIXED_LEN + 8 * n;
    assert_eq!(
        file_length(&path),
        expected_footer_offset + expected_footer_length + TRAILER_LEN
    );

    let bytes = std::fs::read(&path).expect("failed to read file");
    assert_eq!(&bytes[0..8], &MAGIC);
    let trailer = &bytes[bytes.len() - 16..];
    assert_eq!(
        u64::from_le_bytes(trailer[0..8].try_into().unwrap()),
        expected_footer_length
    );
    assert_eq!(
        u64::from_le_bytes(trailer[8..16].try_into().unwrap()),
        expected_footer_offset
    );
}

#[test]
fn header_is_written_at_creation_before_any_block() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard.tmp");
    let header = header_for(4, 2, 5, 1 << 20);
    let _writer =
        ShardFileWriter::create(&path, header.clone()).expect("failed to create shard file");

    // Nothing appended, nothing finished, yet the file identifies itself.
    let page = std::fs::read(&path).expect("failed to read file");
    assert_eq!(page.len() as u64, HEADER_LEN);
    let decoded = ShardFileHeader::decode(&page).expect("failed to decode header");
    assert_eq!(decoded, header);
}

#[test]
fn round_trip_reads_back_every_block_with_its_checksum() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    for &(k, m, index, block_length, full_blocks, last_len) in &[
        (1u8, 1u8, 0u8, 4096u64, 0usize, 1usize),
        (3, 1, 3, 4096, 2, 4096),
        (4, 2, 1, 8192, 5, 17),
        (10, 4, 13, 4096, 1, 4095),
    ] {
        let path = dir.path().join(format!("shard-{k}-{m}-{index}"));
        let header = header_for(k, m, index, block_length);
        let written = write_shard_file(&path, header.clone(), full_blocks, last_len);

        let reader = ShardFileReader::open(&path).expect("failed to open shard file");
        assert_eq!(reader.header(), &header);
        assert_eq!(reader.block_count(), written.len() as u64);
        assert_eq!(reader.footer().last_block_length, last_len as u64);
        assert_eq!(
            reader.footer().object_size,
            object_size_for(&header, full_blocks, last_len)
        );
        assert_eq!(reader.footer().object_checksum, FAKE_OBJECT_CHECKSUM);
        for (i, expected) in written.iter().enumerate() {
            let block = reader.read_block(i as u64).expect("failed to read block");
            assert_eq!(&block, expected, "block {i}");
            assert!(block_matches_checksum(&block.bytes, block.checksum));
        }
        assert!(matches!(
            reader.read_block(written.len() as u64),
            Err(ShardFileError::BlockOutOfRange { .. })
        ));
    }
}

#[test]
fn writer_rejects_wrong_index_bad_lengths_bad_checksums_and_blocks_after_a_short_one() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    let mut writer = ShardFileWriter::create(&path, header_for(3, 1, 1, 4096))
        .expect("failed to create shard file");

    assert!(matches!(
        writer.append_block(&block_for(2, 4096, 1)),
        Err(ShardFileError::WrongShardIndex { .. })
    ));
    assert!(matches!(
        writer.append_block(&block_for(1, 4097, 1)),
        Err(ShardFileError::BadBlockLength { .. })
    ));
    assert!(matches!(
        writer.append_block(&block_for(1, 0, 1)),
        Err(ShardFileError::BadBlockLength { .. })
    ));
    let mut lying = block_for(1, 4096, 1);
    lying.checksum = BlockChecksum(lying.checksum.0 ^ 1);
    assert!(matches!(
        writer.append_block(&lying),
        Err(ShardFileError::SuppliedChecksumMismatch { .. })
    ));

    writer
        .append_block(&block_for(1, 4096, 2))
        .expect("failed to append full block");
    writer
        .append_block(&block_for(1, 100, 3))
        .expect("failed to append short block");
    assert!(matches!(
        writer.append_block(&block_for(1, 4096, 4)),
        Err(ShardFileError::BlockAfterShortBlock)
    ));
    assert_eq!(writer.blocks_written(), 2);
}

#[test]
fn finishing_with_no_blocks_is_an_error() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    let writer = ShardFileWriter::create(&path, header_for(3, 1, 0, 4096))
        .expect("failed to create shard file");
    assert!(matches!(
        writer.finish(0, FAKE_OBJECT_CHECKSUM),
        Err(ShardFileError::NoBlocks)
    ));
}

#[test]
fn truncated_or_extended_files_fail_the_trailer_invariant() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    write_shard_file(&path, header_for(3, 1, 0, 4096), 2, 4096);
    let original = std::fs::read(&path).expect("failed to read file");

    // Truncate by one byte: the trailer is now garbage.
    std::fs::write(&path, &original[..original.len() - 1]).expect("failed to write file");
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::TrailerInvariant { .. })
    ));

    // Append one byte: the trailer is intact but no longer at the end.
    let mut extended = original.clone();
    extended.push(0);
    std::fs::write(&path, &extended).expect("failed to write file");
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::TrailerInvariant { .. })
    ));

    // Far too short to be a shard file at all.
    std::fs::write(&path, &original[..100]).expect("failed to write file");
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::TooShort(100))
    ));
}

#[test]
fn a_corrupt_footer_or_trailer_is_detected_by_the_footer_checksum() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    write_shard_file(&path, header_for(3, 1, 0, 4096), 2, 4096);
    let len = file_length(&path);
    let footer_offset = HEADER_LEN + 3 * 4096;

    // A checksum table entry.
    flip_byte_at(&path, footer_offset + FOOTER_FIXED_LEN + 3);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::FooterChecksumMismatch)
    ));
    flip_byte_at(&path, footer_offset + FOOTER_FIXED_LEN + 3);
    ShardFileReader::open(&path).expect("file should be intact again");

    // The object size field.
    flip_byte_at(&path, footer_offset + 16);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::FooterChecksumMismatch)
    ));
    flip_byte_at(&path, footer_offset + 16);

    // The trailer's footer length: the invariant catches it first.
    flip_byte_at(&path, len - 16);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::TrailerInvariant { .. })
    ));
}

#[test]
fn a_corrupt_header_is_detected_by_the_header_checksum_or_magic() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    write_shard_file(&path, header_for(3, 1, 0, 4096), 2, 4096);

    // The shard index byte.
    flip_byte_at(&path, 22);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::HeaderChecksumMismatch)
    ));
    flip_byte_at(&path, 22);

    // The key hash.
    flip_byte_at(&path, 40);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::HeaderChecksumMismatch)
    ));
    flip_byte_at(&path, 40);

    // The magic.
    flip_byte_at(&path, 0);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::BadMagic)
    ));
    flip_byte_at(&path, 0);
    ShardFileReader::open(&path).expect("file should be intact again");
}

#[test]
fn a_corrupt_block_is_not_detected_on_open_but_fails_its_checksum() {
    // Opening verifies the file's structure, not the blocks; that is the
    // stripe decoder's job, and this is what makes opening cheap.
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    write_shard_file(&path, header_for(3, 1, 0, 4096), 2, 4096);
    flip_byte_at(&path, HEADER_LEN + 4096 + 77); // inside block 1

    let reader = ShardFileReader::open(&path).expect("open should not verify blocks");
    let good = reader.read_block(0).expect("failed to read block 0");
    assert!(block_matches_checksum(&good.bytes, good.checksum));
    let bad = reader.read_block(1).expect("failed to read block 1");
    assert!(!block_matches_checksum(&bad.bytes, bad.checksum));
}

#[test]
fn header_and_footer_must_agree_on_geometry() {
    // Rewrite the header with a different block length but a valid
    // checksum. The trailer and footer still verify, but the parts disagree
    // about where the blocks are.
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    write_shard_file(&path, header_for(3, 1, 0, 4096), 2, 4096);
    let forged = header_for(3, 1, 0, 8192).encode();
    let mut file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("failed to open file");
    file.seek(SeekFrom::Start(0)).expect("failed to seek");
    file.write_all(&forged)
        .expect("failed to write forged header");
    drop(file);
    assert!(matches!(
        ShardFileReader::open(&path),
        Err(ShardFileError::InconsistentFooter(_))
    ));
}

#[test]
fn object_round_trips_through_stripes_and_shard_files_on_disk() {
    // The whole milestone 1 path: object bytes -> stripes -> k+m shard
    // files on disk -> read back -> stripes -> object bytes. Then damage
    // one file and do it again.
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let scheme = Scheme::new(3, 1).expect("failed to construct scheme");
    let code = ReedSolomonCode::new(scheme);
    let block_size: usize = 4096;
    let stripe_size = scheme.data_shards() * block_size;
    let object = xorshift64_bytes(10 * stripe_size + 1234, 42);
    let key_hash = hash_key(b"docs/report.pdf");
    let version_id = VersionId([7u8; 16]);
    let object_checksum = checksum_block(&object);

    // Write: one writer per shard, fed stripe by stripe.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut writers: Vec<ShardFileWriter> = Vec::new();
    for index in scheme.shard_indices() {
        let path = dir.path().join(format!("v.{}.shard", index.0));
        let header = ShardFileHeader {
            scheme,
            shard_index: index,
            block_length: block_size as u64,
            key_hash,
            version_id,
        };
        writers.push(ShardFileWriter::create(&path, header).expect("failed to create shard file"));
        paths.push(path);
    }
    for stripe in object.chunks(stripe_size) {
        let blocks = encode_stripe(&code, stripe, block_size).expect("failed to encode stripe");
        for block in &blocks {
            writers[block.index.as_usize()]
                .append_block(block)
                .expect("failed to append block");
        }
    }
    for writer in writers {
        writer
            .finish(object.len() as u64, object_checksum)
            .expect("failed to finish shard file");
    }

    // Read: the read path, data shards only.
    let readers: Vec<ShardFileReader> = paths
        .iter()
        .map(|p| ShardFileReader::open(p).expect("failed to open shard file"))
        .collect();
    let stripe_count = readers[0].block_count();
    let data_indices = scheme.data_shard_indices();
    let mut recovered = Vec::with_capacity(object.len());
    for stripe_number in 0..stripe_count {
        let mut received = Vec::with_capacity(data_indices.len());
        for index in &data_indices {
            received.push(
                readers[index.as_usize()]
                    .read_block(stripe_number)
                    .expect("failed to read block"),
            );
        }
        let stripe_start = stripe_number as usize * stripe_size;
        let stripe_len = (object.len() - stripe_start).min(stripe_size);
        match decode_stripe(&code, &data_indices, &received, stripe_len)
            .expect("failed to decode stripe")
        {
            DecodedStripe::Intact { data } => recovered.extend_from_slice(&data),
            other => panic!("stripe {stripe_number}: expected Intact, got {other:?}"),
        }
    }
    assert_eq!(recovered, object);
    // The whole-object check the read path performs (SPEC 11.7), and the
    // copy the recovery tool would use.
    assert_eq!(checksum_block(&recovered), object_checksum);
    for reader in &readers {
        assert_eq!(reader.footer().object_checksum, object_checksum);
        assert_eq!(reader.footer().object_size, object.len() as u64);
    }

    // Damage block 4 of shard 1 on disk, then read every shard and let the
    // decoder repair.
    drop(readers);
    flip_byte_at(&paths[1], HEADER_LEN + 4 * block_size as u64 + 10);
    let readers: Vec<ShardFileReader> = paths
        .iter()
        .map(|p| ShardFileReader::open(p).expect("failed to open shard file"))
        .collect();
    let all_indices = scheme.shard_indices();
    let mut repaired_stripes = 0;
    let mut recovered = Vec::with_capacity(object.len());
    for stripe_number in 0..stripe_count {
        let mut received = Vec::with_capacity(all_indices.len());
        for index in &all_indices {
            received.push(
                readers[index.as_usize()]
                    .read_block(stripe_number)
                    .expect("failed to read block"),
            );
        }
        let stripe_start = stripe_number as usize * stripe_size;
        let stripe_len = (object.len() - stripe_start).min(stripe_size);
        match decode_stripe(&code, &all_indices, &received, stripe_len)
            .expect("failed to decode stripe")
        {
            DecodedStripe::Intact { data } => recovered.extend_from_slice(&data),
            DecodedStripe::Repaired { data, faults } => {
                assert_eq!(stripe_number, 4);
                assert_eq!(faults.len(), 1);
                assert_eq!(faults[0].index, ShardIndex(1));
                repaired_stripes += 1;
                recovered.extend_from_slice(&data);
            }
            other => panic!("stripe {stripe_number}: unexpected {other:?}"),
        }
    }
    assert_eq!(repaired_stripes, 1);
    assert_eq!(recovered, object);
}

#[test]
fn shard_geometry_follows_the_padding_rule() {
    let scheme = Scheme::new(3, 1).expect("failed to construct scheme");
    // Appendix B: 10 MiB at 3+1 with 1 MiB blocks is four stripes, the last
    // holding 1 MiB split into blocks of 349,526 bytes.
    assert_eq!(
        shard_geometry(scheme, 1 << 20, 10 << 20),
        Some(ShardGeometry {
            block_count: 4,
            last_block_length: 349_526
        })
    );
    // Exactly one full stripe.
    assert_eq!(
        shard_geometry(scheme, 4096, 3 * 4096),
        Some(ShardGeometry {
            block_count: 1,
            last_block_length: 4096
        })
    );
    // One byte more than a full stripe: a second stripe of one byte,
    // padded to one-byte blocks.
    assert_eq!(
        shard_geometry(scheme, 4096, 3 * 4096 + 1),
        Some(ShardGeometry {
            block_count: 2,
            last_block_length: 1
        })
    );
    // A two-byte object splits into blocks of one byte.
    assert_eq!(
        shard_geometry(scheme, 4096, 2),
        Some(ShardGeometry {
            block_count: 1,
            last_block_length: 1
        })
    );
    assert_eq!(shard_geometry(scheme, 4096, 0), None);
}

#[test]
fn finish_refuses_blocks_that_do_not_match_the_object_size() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let header = header_for(3, 1, 0, 4096);

    // Too few blocks: two full blocks written, object size says three.
    let path = dir.path().join("too-few");
    let mut writer =
        ShardFileWriter::create(&path, header.clone()).expect("failed to create shard file");
    writer
        .append_block(&block_for(0, 4096, 1))
        .expect("failed to append block");
    writer
        .append_block(&block_for(0, 4096, 2))
        .expect("failed to append block");
    let three_stripes = 3 * 3 * 4096;
    match writer.finish(three_stripes, FAKE_OBJECT_CHECKSUM) {
        Err(ShardFileError::GeometryMismatch {
            expected_blocks,
            actual_blocks,
            ..
        }) => {
            assert_eq!(expected_blocks, 3);
            assert_eq!(actual_blocks, 2);
        }
        other => panic!("expected GeometryMismatch, got {other:?}"),
    }

    // Wrong padding: the object needs a last block of 2 bytes, 1 was written.
    let path = dir.path().join("wrong-last");
    let mut writer =
        ShardFileWriter::create(&path, header.clone()).expect("failed to create shard file");
    writer
        .append_block(&block_for(0, 4096, 1))
        .expect("failed to append block");
    writer
        .append_block(&block_for(0, 1, 2))
        .expect("failed to append block");
    let one_stripe_plus_five = 3 * 4096 + 5; // ceil(5 / 3) = 2
    match writer.finish(one_stripe_plus_five, FAKE_OBJECT_CHECKSUM) {
        Err(ShardFileError::GeometryMismatch {
            expected_last,
            actual_last,
            ..
        }) => {
            assert_eq!(expected_last, 2);
            assert_eq!(actual_last, 1);
        }
        other => panic!("expected GeometryMismatch, got {other:?}"),
    }

    // Object size zero can never match a file with blocks.
    let path = dir.path().join("zero");
    let mut writer = ShardFileWriter::create(&path, header).expect("failed to create shard file");
    writer
        .append_block(&block_for(0, 4096, 1))
        .expect("failed to append block");
    assert!(matches!(
        writer.finish(0, FAKE_OBJECT_CHECKSUM),
        Err(ShardFileError::GeometryMismatch { .. })
    ));
}

#[test]
fn open_refuses_a_footer_whose_object_size_disagrees_with_the_blocks() {
    // Build the file by hand with a correctly checksummed footer that
    // claims an object size the blocks cannot hold. Only the geometry
    // check can catch this.
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("shard");
    let header = header_for(3, 1, 0, 4096);
    let block = block_for(0, 4096, 1);
    let footer = ShardFileFooter {
        block_count: 1,
        last_block_length: 4096,
        object_size: 3 * 4096 * 2, // says two stripes; the file has one block
        object_checksum: FAKE_OBJECT_CHECKSUM,
        checksums: vec![block.checksum],
    };
    let footer_offset = HEADER_LEN + 4096;
    let mut bytes = header.encode();
    bytes.extend_from_slice(&block.bytes);
    bytes.extend_from_slice(&footer.encode_with_trailer(footer_offset));
    std::fs::write(&path, &bytes).expect("failed to write file");

    match ShardFileReader::open(&path) {
        Err(ShardFileError::GeometryMismatch {
            expected_blocks,
            actual_blocks,
            ..
        }) => {
            assert_eq!(expected_blocks, 2);
            assert_eq!(actual_blocks, 1);
        }
        Err(other) => panic!("expected GeometryMismatch, got {other:?}"),
        Ok(_) => panic!("expected GeometryMismatch, but the file opened"),
    }

    // The same file with a truthful object size opens.
    let truthful = ShardFileFooter {
        object_size: 3 * 4096,
        ..footer
    };
    let mut bytes = header.encode();
    bytes.extend_from_slice(&block.bytes);
    bytes.extend_from_slice(&truthful.encode_with_trailer(footer_offset));
    std::fs::write(&path, &bytes).expect("failed to write file");
    ShardFileReader::open(&path).expect("truthful footer should open");
}
