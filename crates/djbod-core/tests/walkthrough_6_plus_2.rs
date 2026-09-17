//! A step-by-step walkthrough of the three layers for one concrete scheme,
//! 6+2, with every block written out. It exists to be read as much as to be
//! run: it shows how a checksum failure becomes an erasure and how the
//! erasure is then repaired, using the layers one call at a time.

use djbod_core::checksum::{block_matches_checksum, checksum_block, BlockChecksum};
use djbod_core::erasure::{Coder, Scheme, ShardIndex};
use djbod_core::stripe::{decode_stripe, encode_stripe, FaultKind, ReceivedBlock};

#[test]
fn six_plus_two_corrupt_block_becomes_an_erasure_and_is_repaired() {
    // Step 1. The scheme: 6 data shards, 2 parity shards, 8 shards total.
    let scheme = Scheme::new(6, 2).expect("failed to construct scheme");
    let coder = Coder::new(scheme);
    assert_eq!(scheme.total_shards(), 8);

    // Step 2. Six data blocks of 8 bytes each. In the real system these are
    // six consecutive runs of the object (SPEC 8.2.3); here they are just
    // recognisable bytes.
    let data_blocks: Vec<Vec<u8>> = vec![
        b"block-0.".to_vec(),
        b"block-1.".to_vec(),
        b"block-2.".to_vec(),
        b"block-3.".to_vec(),
        b"block-4.".to_vec(),
        b"block-5.".to_vec(),
    ];

    // Step 3. Compute the two parity blocks. They are the same length as
    // the data blocks and are shards 6 and 7.
    let parity_blocks = coder
        .compute_parity(&data_blocks)
        .expect("failed to compute parity");
    assert_eq!(parity_blocks.len(), 2);
    assert_eq!(parity_blocks[0].len(), 8);
    assert_eq!(parity_blocks[1].len(), 8);

    // Gather all eight shards in shard index order: 0..5 data, 6..7 parity.
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(8);
    for block in &data_blocks {
        shards.push(block.clone());
    }
    for block in &parity_blocks {
        shards.push(block.clone());
    }

    // Step 4. Checksum every shard. These eight values are what a shard
    // file header stores (SPEC 8.3.3). Nothing else records what "correct"
    // looks like.
    let mut stored_checksums: Vec<BlockChecksum> = Vec::with_capacity(8);
    for shard in &shards {
        stored_checksums.push(checksum_block(shard));
    }

    // Step 5. Verify all eight against their stored checksums. All good.
    for i in 0..8 {
        assert!(
            block_matches_checksum(&shards[i], stored_checksums[i]),
            "shard {i} should match its checksum before any damage"
        );
    }

    // Step 6. Flip one bit in shard 2 (the third data block). This is what
    // bitrot on the disk holding shard 2 looks like.
    let original_block_2 = shards[2].clone();
    shards[2][5] ^= 0b0000_0100;
    assert_ne!(shards[2], original_block_2);

    // Step 7. Verify again. Exactly one shard fails, and we know which.
    // The parity was not consulted to find this out (SPEC 8.3.5).
    let mut bad_shards: Vec<ShardIndex> = Vec::new();
    for i in 0..8 {
        if !block_matches_checksum(&shards[i], stored_checksums[i]) {
            bad_shards.push(ShardIndex(i as u8));
        }
    }
    assert_eq!(bad_shards, vec![ShardIndex(2)]);

    // Step 8. Treat shard 2 as erased: hand the coder only the seven shards
    // that passed, and ask for shard 2 back. Seven is more than the six
    // needed, so the coder has a spare.
    let mut present: Vec<(ShardIndex, &[u8])> = Vec::with_capacity(7);
    for i in 0..8 {
        if i != 2 {
            present.push((ShardIndex(i as u8), &shards[i]));
        }
    }
    let recovered = coder
        .reconstruct(&present, &[ShardIndex(2)])
        .expect("failed to reconstruct shard 2");

    // Step 9. The reconstructed block is the original, and it matches the
    // stored checksum, which is how a repair job would confirm its work
    // before writing it back (SPEC 18.4).
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0], original_block_2);
    assert_eq!(recovered[0], b"block-2.");
    assert!(block_matches_checksum(&recovered[0], stored_checksums[2]));
}

#[test]
fn the_same_walkthrough_through_the_stripe_layer() {
    // The stripe layer does steps 2 to 9 in two calls. This test shows
    // what it reports, and in particular the behaviour behind the open API
    // question: faults are judged against the whole scheme, so blocks the
    // caller never asked for are reported as Missing.
    let coder = Coder::new(Scheme::new(6, 2).expect("failed to construct scheme"));
    let object_bytes = b"block-0.block-1.block-2.block-3.block-4.block-5.";
    let encoded = encode_stripe(&coder, object_bytes, 8).expect("failed to encode stripe");
    assert_eq!(encoded.blocks[2], b"block-2.");

    // Case A: the read path fetches only the six data blocks (SPEC 11.3),
    // and one of them is corrupt.
    let mut received: Vec<ReceivedBlock> = Vec::with_capacity(6);
    for i in 0..6 {
        received.push(ReceivedBlock {
            index: ShardIndex(i as u8),
            bytes: encoded.blocks[i].clone(),
            stored_checksum: encoded.checksums[i],
        });
    }
    received[2].bytes[5] ^= 0b0000_0100;

    // With only five usable blocks of the six needed, this cannot decode.
    // The error lists shard 2 as a checksum mismatch and shards 6 and 7 as
    // Missing, although the caller chose not to fetch 6 and 7.
    let err = decode_stripe(&coder, &received, object_bytes.len())
        .expect_err("five usable blocks cannot decode");
    let faults = match err {
        djbod_core::stripe::StripeError::Unrecoverable {
            usable,
            needed,
            faults,
        } => {
            assert_eq!(usable, 5);
            assert_eq!(needed, 6);
            faults
        }
        other => panic!("expected Unrecoverable, got {other:?}"),
    };
    assert_eq!(faults.len(), 3);
    assert_eq!(faults[0].index, ShardIndex(2));
    assert!(matches!(faults[0].kind, FaultKind::ChecksumMismatch { .. }));
    assert_eq!(faults[1].index, ShardIndex(6));
    assert_eq!(faults[1].kind, FaultKind::Missing);
    assert_eq!(faults[2].index, ShardIndex(7));
    assert_eq!(faults[2].kind, FaultKind::Missing);

    // Case B: the caller goes back for one parity block. Now six blocks are
    // usable and the data decodes exactly. Shard 7 is still reported
    // Missing even though nothing was wrong with it; it was simply not
    // fetched. This is the report a fail-stop caller would have to filter.
    received.push(ReceivedBlock {
        index: ShardIndex(6),
        bytes: encoded.blocks[6].clone(),
        stored_checksum: encoded.checksums[6],
    });
    let decoded =
        decode_stripe(&coder, &received, object_bytes.len()).expect("failed to decode stripe");
    assert_eq!(decoded.data, object_bytes);
    assert_eq!(decoded.faults.len(), 2);
    assert_eq!(decoded.faults[0].index, ShardIndex(2));
    assert_eq!(decoded.faults[1].index, ShardIndex(7));
    assert_eq!(decoded.faults[1].kind, FaultKind::Missing);
}
