//! Tests of stripe encoding and of decoding under damage. These prove the
//! property SPEC 8.3 rests on: the checksum converts corruption into an
//! erasure at a known position, which the code can then repair, and the
//! decoder reports exactly which shards were damaged.

use djbod_core::checksum::checksum_block;
use djbod_core::erasure::{CodingError, ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::stripe::{
    block_length_for, decode_stripe, encode_stripe, BlockFault, DecodedStripe, EncodedStripe,
    FaultKind, ReceivedBlock, StripeError,
};

const SCHEMES: &[(u8, u8)] = &[
    (1, 1),
    (1, 2),
    (2, 1),
    (3, 1),
    (3, 2),
    (4, 2),
    (6, 3),
    (10, 4),
];
const BLOCK_SIZE: usize = 4096;

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

fn reed_solomon_code_for(k: u8, m: u8) -> ReedSolomonCode {
    ReedSolomonCode::new(Scheme::new(k, m).expect("failed to construct scheme"))
}

/// Every block of an encoded stripe as the decoder would receive it from
/// an honest holder.
fn all_received(encoded: &EncodedStripe) -> Vec<ReceivedBlock> {
    let mut received = Vec::with_capacity(encoded.blocks.len());
    for i in 0..encoded.blocks.len() {
        received.push(ReceivedBlock {
            index: ShardIndex(i as u8),
            bytes: encoded.blocks[i].clone(),
            stored_checksum: encoded.checksums[i],
        });
    }
    received
}

/// Every subset of `n` indices of size `size`.
fn combinations(n: usize, size: usize) -> Vec<Vec<usize>> {
    fn go(start: usize, n: usize, size: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == size {
            out.push(cur.clone());
            return;
        }
        for i in start..n {
            cur.push(i);
            go(i + 1, n, size, cur, out);
            cur.pop();
        }
    }
    let mut out = Vec::new();
    go(0, n, size, &mut Vec::new(), &mut out);
    out
}

#[test]
fn full_stripe_splits_contiguously_and_data_blocks_are_verbatim() {
    for &(k, m) in SCHEMES {
        let code = reed_solomon_code_for(k, m);
        let data = xorshift64_bytes(k as usize * BLOCK_SIZE, 1);
        let encoded = encode_stripe(&code, &data, BLOCK_SIZE).expect("failed to encode stripe");

        assert_eq!(encoded.blocks.len(), (k + m) as usize);
        assert_eq!(encoded.checksums.len(), (k + m) as usize);
        assert_eq!(encoded.block_len(), BLOCK_SIZE);
        assert_eq!(encoded.data_len, data.len());
        for i in 0..k as usize {
            let expected = &data[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            assert_eq!(
                encoded.blocks[i], expected,
                "scheme {k}+{m}: data block {i} is not a verbatim run"
            );
            assert_eq!(encoded.checksums[i], checksum_block(expected));
        }
    }
}

#[test]
fn short_final_stripe_is_padded_to_equal_blocks() {
    // 10 MiB object at 3+1 with 1 MiB blocks: the fourth stripe holds
    // 1 MiB and each block is ceil(1 MiB / 3) = 349,526 bytes (Appendix B).
    let code = reed_solomon_code_for(3, 1);
    let block_size = 1 << 20;
    let data = xorshift64_bytes(1 << 20, 2);
    let encoded = encode_stripe(&code, &data, block_size).expect("failed to encode stripe");
    assert_eq!(encoded.block_len(), 349_526);
    assert_eq!(block_length_for(&code, data.len()), 349_526);
    // The last data block ends with padding.
    let padding = 3 * 349_526 - data.len();
    assert_eq!(padding, 2);
    let last = &encoded.blocks[2];
    assert_eq!(&last[last.len() - padding..], &[0u8, 0u8]);
    // Round trip strips it.
    let result = decode_stripe(
        &code,
        &code.scheme().shard_indices(),
        &all_received(&encoded),
        data.len(),
    )
    .expect("failed to decode stripe");
    assert_eq!(result, DecodedStripe::Intact { data });
}

#[test]
fn stripes_shorter_than_k_bytes_still_encode() {
    let code = reed_solomon_code_for(4, 2);
    for len in 1..=5 {
        let data = xorshift64_bytes(len, len as u64);
        let encoded = encode_stripe(&code, &data, BLOCK_SIZE).expect("failed to encode stripe");
        assert_eq!(
            encoded.block_len(),
            if len <= 4 { 1 } else { 2 },
            "len {len}"
        );
        let result = decode_stripe(
            &code,
            &code.scheme().shard_indices(),
            &all_received(&encoded),
            len,
        )
        .expect("failed to decode stripe");
        assert_eq!(result, DecodedStripe::Intact { data }, "len {len}");
    }
}

#[test]
fn empty_and_oversized_stripes_are_rejected() {
    let code = reed_solomon_code_for(3, 1);
    assert_eq!(
        encode_stripe(&code, &[], BLOCK_SIZE),
        Err(StripeError::Empty)
    );
    let too_big = vec![0u8; 3 * BLOCK_SIZE + 1];
    assert_eq!(
        encode_stripe(&code, &too_big, BLOCK_SIZE),
        Err(StripeError::TooLarge {
            actual: 3 * BLOCK_SIZE + 1,
            max: 3 * BLOCK_SIZE
        })
    );
    assert_eq!(decode_stripe(&code, &[], &[], 0), Err(StripeError::Empty));
}

#[test]
fn intact_blocks_decode_with_no_faults_and_no_parity_needed() {
    for &(k, m) in SCHEMES {
        let code = reed_solomon_code_for(k, m);
        let data = xorshift64_bytes(k as usize * BLOCK_SIZE - 17, 3);
        let encoded = encode_stripe(&code, &data, BLOCK_SIZE).expect("failed to encode stripe");

        // Request and hand over only the data blocks, as the read path
        // does (11.3). Nothing is reported: the parity was never asked for.
        let mut received = all_received(&encoded);
        received.truncate(k as usize);
        let result = decode_stripe(
            &code,
            &code.scheme().data_shard_indices(),
            &received,
            data.len(),
        )
        .expect("failed to decode stripe");
        assert_eq!(result, DecodedStripe::Intact { data }, "scheme {k}+{m}");
    }
}

#[test]
fn a_flipped_bit_is_reported_as_a_checksum_mismatch_and_repaired() {
    for &(k, m) in SCHEMES {
        let code = reed_solomon_code_for(k, m);
        let data = xorshift64_bytes(k as usize * BLOCK_SIZE, 4);
        let encoded = encode_stripe(&code, &data, BLOCK_SIZE).expect("failed to encode stripe");

        for victim in 0..(k + m) as usize {
            let mut received = all_received(&encoded);
            received[victim].bytes[BLOCK_SIZE / 2] ^= 0x10;

            let result =
                decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
                    .expect("failed to decode stripe");
            let faults = match result {
                DecodedStripe::Repaired {
                    data: recovered,
                    faults,
                } => {
                    assert_eq!(recovered, data, "scheme {k}+{m}, corrupt shard {victim}");
                    faults
                }
                other => panic!(
                    "scheme {k}+{m}, corrupt shard {victim}: expected Repaired, got {other:?}"
                ),
            };
            assert_eq!(faults.len(), 1);
            let fault = &faults[0];
            assert_eq!(fault.index, ShardIndex(victim as u8));
            match &fault.kind {
                FaultKind::ChecksumMismatch { stored, computed } => {
                    assert_eq!(*stored, encoded.checksums[victim]);
                    assert_eq!(*computed, checksum_block(&received[victim].bytes));
                    assert_ne!(stored, computed);
                }
                other => panic!("expected ChecksumMismatch, got {other:?}"),
            }
        }
    }
}

#[test]
fn every_damage_pattern_up_to_m_is_repaired_and_reported_exactly() {
    // Damage is a mix: the first index in each pattern is dropped, the
    // second (if any) gets a flipped bit, the third (if any) is truncated,
    // and so on cyclically. Each kind must appear in the fault list with
    // the right index and kind, and the data must come back exact.
    for &(k, m) in SCHEMES {
        let code = reed_solomon_code_for(k, m);
        let n = (k + m) as usize;
        let data = xorshift64_bytes(k as usize * 256, 5);
        let encoded = encode_stripe(&code, &data, 256).expect("failed to encode stripe");

        for damaged in 1..=m as usize {
            for pattern in combinations(n, damaged) {
                let mut received = all_received(&encoded);
                let mut expected_kinds: Vec<(ShardIndex, FaultKind)> = Vec::new();
                for (position, &victim) in pattern.iter().enumerate() {
                    let index = ShardIndex(victim as u8);
                    match position % 3 {
                        0 => {
                            expected_kinds.push((index, FaultKind::Missing));
                        }
                        1 => {
                            received[victim].bytes[0] ^= 0xFF;
                            expected_kinds.push((
                                index,
                                FaultKind::ChecksumMismatch {
                                    stored: encoded.checksums[victim],
                                    computed: checksum_block(&received[victim].bytes),
                                },
                            ));
                        }
                        _ => {
                            received[victim].bytes.pop();
                            expected_kinds.push((
                                index,
                                FaultKind::WrongLength {
                                    expected: 256,
                                    actual: 255,
                                },
                            ));
                        }
                    }
                }
                // Apply the drops last so the other damage above indexed the
                // original positions.
                let mut kept = Vec::with_capacity(n);
                for block in received {
                    let mut dropped = false;
                    for (position, &victim) in pattern.iter().enumerate() {
                        if position % 3 == 0 && block.index == ShardIndex(victim as u8) {
                            dropped = true;
                        }
                    }
                    if !dropped {
                        kept.push(block);
                    }
                }

                let result =
                    decode_stripe(&code, &code.scheme().shard_indices(), &kept, data.len())
                        .expect("failed to decode stripe");
                let faults = match result {
                    DecodedStripe::Repaired {
                        data: recovered,
                        faults,
                    } => {
                        assert_eq!(recovered, data, "scheme {k}+{m}, pattern {pattern:?}");
                        faults
                    }
                    other => panic!(
                        "scheme {k}+{m}, pattern {pattern:?}: expected Repaired, got {other:?}"
                    ),
                };

                let mut reported: Vec<(ShardIndex, FaultKind)> = Vec::new();
                for fault in faults {
                    reported.push((fault.index, fault.kind));
                }
                assert_eq!(
                    reported, expected_kinds,
                    "scheme {k}+{m}, pattern {pattern:?}"
                );
            }
        }
    }
}

#[test]
fn more_than_m_damaged_blocks_is_unrecoverable_listing_every_fault() {
    for &(k, m) in SCHEMES {
        let code = reed_solomon_code_for(k, m);
        let data = xorshift64_bytes(k as usize * 128, 6);
        let encoded = encode_stripe(&code, &data, 128).expect("failed to encode stripe");

        // Corrupt m + 1 blocks in place; nothing is dropped.
        let mut received = all_received(&encoded);
        for i in 0..=m as usize {
            received[i].bytes[7] ^= 0x01;
        }
        let result = decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
            .expect("failed to decode stripe");
        match result {
            DecodedStripe::Unrecoverable {
                usable,
                needed,
                faults,
            } => {
                assert_eq!(usable, k as usize - 1, "scheme {k}+{m}");
                assert_eq!(needed, k as usize);
                assert_eq!(faults.len(), m as usize + 1);
                for (position, fault) in faults.iter().enumerate() {
                    assert_eq!(fault.index, ShardIndex(position as u8));
                    assert!(matches!(fault.kind, FaultKind::ChecksumMismatch { .. }));
                }
            }
            other => panic!("scheme {k}+{m}: expected Unrecoverable, got {other:?}"),
        }
    }
}

#[test]
fn swapped_blocks_are_both_caught_by_their_checksums() {
    // Two holders return each other's block. Both blocks are intact bytes
    // but neither matches the checksum stored for its index.
    let code = reed_solomon_code_for(4, 2);
    let data = xorshift64_bytes(4 * 512, 8);
    let encoded = encode_stripe(&code, &data, 512).expect("failed to encode stripe");
    let mut received = all_received(&encoded);
    let a = received[1].bytes.clone();
    let b = received[2].bytes.clone();
    received[1].bytes = b;
    received[2].bytes = a;

    let result = decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
        .expect("failed to decode stripe");
    let faults = match result {
        DecodedStripe::Repaired {
            data: recovered,
            faults,
        } => {
            assert_eq!(recovered, data);
            faults
        }
        other => panic!("expected Repaired, got {other:?}"),
    };
    assert_eq!(faults.len(), 2);
    assert_eq!(faults[0].index, ShardIndex(1));
    assert_eq!(faults[1].index, ShardIndex(2));
    for fault in &faults {
        assert!(matches!(fault.kind, FaultKind::ChecksumMismatch { .. }));
    }
}

#[test]
fn no_parity_scheme_round_trips_and_cannot_repair() {
    let code = reed_solomon_code_for(3, 0);
    let data = xorshift64_bytes(3 * 1000, 9);
    let encoded = encode_stripe(&code, &data, 1000).expect("failed to encode stripe");
    assert_eq!(encoded.blocks.len(), 3);

    let result = decode_stripe(
        &code,
        &code.scheme().shard_indices(),
        &all_received(&encoded),
        data.len(),
    )
    .expect("failed to decode stripe");
    assert_eq!(result, DecodedStripe::Intact { data: data.clone() });

    let mut received = all_received(&encoded);
    received[0].bytes[0] ^= 0x01;
    let result = decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
        .expect("failed to decode stripe");
    assert!(matches!(
        result,
        DecodedStripe::Unrecoverable {
            usable: 2,
            needed: 3,
            ..
        }
    ));
}

#[test]
fn a_wrong_stored_checksum_condemns_a_good_block() {
    // Corruption of the checksum table has the same effect as corruption
    // of the block: the block is treated as erased. The system never
    // guesses which of the two is wrong.
    let code = reed_solomon_code_for(3, 1);
    let data = xorshift64_bytes(3 * 64, 10);
    let encoded = encode_stripe(&code, &data, 64).expect("failed to encode stripe");
    let mut received = all_received(&encoded);
    received[0].stored_checksum =
        djbod_core::checksum::BlockChecksum(received[0].stored_checksum.0 ^ 1);

    let result = decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
        .expect("failed to decode stripe");
    match result {
        DecodedStripe::Repaired {
            data: recovered,
            faults,
        } => {
            assert_eq!(recovered, data);
            assert_eq!(faults.len(), 1);
            assert_eq!(faults[0].index, ShardIndex(0));
        }
        other => panic!("expected Repaired, got {other:?}"),
    }
}

#[test]
fn protocol_errors_are_distinguished_from_faults() {
    let code = reed_solomon_code_for(2, 1);
    let data = xorshift64_bytes(2 * 32, 11);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");

    let mut duplicate = all_received(&encoded);
    duplicate.push(duplicate[0].clone());
    assert_eq!(
        decode_stripe(
            &code,
            &code.scheme().shard_indices(),
            &duplicate,
            data.len()
        ),
        Err(StripeError::Coding(CodingError::DuplicateIndex(
            ShardIndex(0)
        )))
    );

    let mut out_of_range = all_received(&encoded);
    out_of_range[0].index = ShardIndex(9);
    assert_eq!(
        decode_stripe(
            &code,
            &code.scheme().shard_indices(),
            &out_of_range,
            data.len()
        ),
        Err(StripeError::Coding(CodingError::IndexOutOfRange(
            ShardIndex(9)
        )))
    );
}

#[test]
fn fault_list_is_in_shard_index_order_regardless_of_arrival_order() {
    let code = reed_solomon_code_for(4, 2);
    let data = xorshift64_bytes(4 * 16, 12);
    let encoded = encode_stripe(&code, &data, 16).expect("failed to encode stripe");
    let mut received = all_received(&encoded);
    received.reverse();
    received.remove(1); // drops shard index 4
    received[0].bytes[0] ^= 0x01; // corrupts shard index 5
    let result = decode_stripe(&code, &code.scheme().shard_indices(), &received, data.len())
        .expect("failed to decode stripe");
    let faults: Vec<BlockFault> = match result {
        DecodedStripe::Repaired {
            data: recovered,
            faults,
        } => {
            assert_eq!(recovered, data);
            faults
        }
        other => panic!("expected Repaired, got {other:?}"),
    };
    assert_eq!(faults[0].index, ShardIndex(4));
    assert_eq!(faults[0].kind, FaultKind::Missing);
    assert_eq!(faults[1].index, ShardIndex(5));
    assert!(matches!(faults[1].kind, FaultKind::ChecksumMismatch { .. }));
}

#[test]
fn only_requested_indices_can_be_missing() {
    // Request five of six shards and receive four of them. The one
    // requested but absent shard is Missing; the one never requested is
    // not mentioned.
    let code = reed_solomon_code_for(4, 2);
    let data = xorshift64_bytes(4 * 32, 13);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");
    let requested = [
        ShardIndex(0),
        ShardIndex(1),
        ShardIndex(2),
        ShardIndex(3),
        ShardIndex(4),
    ];
    let mut received = all_received(&encoded);
    received.retain(|block| block.index != ShardIndex(2) && block.index != ShardIndex(5));
    assert_eq!(received.len(), 4);

    let result =
        decode_stripe(&code, &requested, &received, data.len()).expect("failed to decode stripe");
    match result {
        DecodedStripe::Repaired {
            data: recovered,
            faults,
        } => {
            assert_eq!(recovered, data);
            assert_eq!(faults.len(), 1);
            assert_eq!(faults[0].index, ShardIndex(2));
            assert_eq!(faults[0].kind, FaultKind::Missing);
        }
        other => panic!("expected Repaired, got {other:?}"),
    }
}

#[test]
fn a_block_that_was_not_requested_is_a_protocol_error() {
    let code = reed_solomon_code_for(2, 1);
    let data = xorshift64_bytes(2 * 32, 14);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");
    let received = all_received(&encoded); // includes parity shard 2
    assert_eq!(
        decode_stripe(
            &code,
            &code.scheme().data_shard_indices(),
            &received,
            data.len()
        ),
        Err(StripeError::UnrequestedBlock(ShardIndex(2)))
    );
}

#[test]
fn a_bad_requested_list_is_a_protocol_error() {
    let code = reed_solomon_code_for(2, 1);
    let data = xorshift64_bytes(2 * 32, 15);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");
    let received = all_received(&encoded);
    assert_eq!(
        decode_stripe(
            &code,
            &[ShardIndex(0), ShardIndex(0)],
            &received,
            data.len()
        ),
        Err(StripeError::Coding(CodingError::DuplicateIndex(
            ShardIndex(0)
        )))
    );
    assert_eq!(
        decode_stripe(
            &code,
            &[ShardIndex(0), ShardIndex(3)],
            &received,
            data.len()
        ),
        Err(StripeError::Coding(CodingError::IndexOutOfRange(
            ShardIndex(3)
        )))
    );
}

#[test]
fn requesting_fewer_than_k_blocks_cannot_decode_even_if_all_arrive() {
    let code = reed_solomon_code_for(3, 1);
    let data = xorshift64_bytes(3 * 32, 16);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");
    let requested = [ShardIndex(0), ShardIndex(1)];
    let mut received = all_received(&encoded);
    received.truncate(2);
    let result =
        decode_stripe(&code, &requested, &received, data.len()).expect("failed to decode stripe");
    assert!(matches!(
        result,
        DecodedStripe::Unrecoverable {
            usable: 2,
            needed: 3,
            ..
        }
    ));
}

#[test]
fn the_three_outcomes_of_decoding() {
    // One stripe, three decodes. This test exists to show the shape of the
    // result: the data is only reachable by naming the case.
    let code = reed_solomon_code_for(3, 1);
    let data = xorshift64_bytes(3 * 32, 17);
    let encoded = encode_stripe(&code, &data, 32).expect("failed to encode stripe");
    let requested = code.scheme().shard_indices();

    // Intact: every requested block arrived and verified.
    let result = decode_stripe(&code, &requested, &all_received(&encoded), data.len())
        .expect("failed to decode stripe");
    assert_eq!(result, DecodedStripe::Intact { data: data.clone() });

    // Repaired: one block corrupt, the parity covers it. The data is exact
    // and the fault list says what to rewrite.
    let mut one_bad = all_received(&encoded);
    one_bad[1].bytes[3] ^= 0x80;
    let result =
        decode_stripe(&code, &requested, &one_bad, data.len()).expect("failed to decode stripe");
    match &result {
        DecodedStripe::Repaired {
            data: recovered,
            faults,
        } => {
            assert_eq!(*recovered, data);
            assert_eq!(faults.len(), 1);
            assert_eq!(faults[0].index, ShardIndex(1));
        }
        other => panic!("expected Repaired, got {other:?}"),
    }

    // Unrecoverable: two blocks corrupt, one parity. No data at all.
    let mut two_bad = all_received(&encoded);
    two_bad[1].bytes[3] ^= 0x80;
    two_bad[2].bytes[3] ^= 0x80;
    let result =
        decode_stripe(&code, &requested, &two_bad, data.len()).expect("failed to decode stripe");
    match &result {
        DecodedStripe::Unrecoverable {
            usable,
            needed,
            faults,
        } => {
            assert_eq!(*usable, 2);
            assert_eq!(*needed, 3);
            assert_eq!(faults.len(), 2);
        }
        other => panic!("expected Unrecoverable, got {other:?}"),
    }

    // Err is reserved for misuse, not for damaged data.
    assert_eq!(
        decode_stripe(&code, &requested, &[], 0),
        Err(StripeError::Empty)
    );
}
