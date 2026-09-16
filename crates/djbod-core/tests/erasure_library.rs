//! Tests of the `reed-solomon-erasure` crate as a candidate library for
//! SPEC.md section 8. Everything here is in memory. The purpose is to
//! confirm, before building on it, that the library behaves as the
//! specification assumes:
//!
//! - systematic: data shards are stored verbatim (8.1.1);
//! - any k of k+m shards recover the stripe, for every erasure pattern
//!   up to m (8.1.1);
//! - m+1 erasures are refused rather than silently wrong;
//! - a corrupt shard that is *not* marked as an erasure is not detected by
//!   reconstruction and produces wrong output, which is why checksums exist
//!   (8.3.4, 8.3.5);
//! - encoding is byte-wise independent, so streaming stripes is valid
//!   (8.2.4);
//! - the first m' parity shards of scheme k+m equal the parity shards of
//!   scheme k+m' for m' < m (8.1.5), which makes reducing m a deletion;
//! - k = 1 yields plain replication (8.1.2);
//! - m = 0 is not supported by the library and must be special-cased by
//!   our wrapper (8.1.2).

use reed_solomon_erasure::galois_8::ReedSolomon;
use reed_solomon_erasure::Error;

/// Schemes exercised by the parameterised tests. Covers replication,
/// RAID 5 and 6 equivalents, the 3+1 from the worked example, and a wide
/// scheme.
const SCHEMES: &[(usize, usize)] = &[
    (1, 1),
    (1, 2),
    (2, 1),
    (3, 1),
    (3, 2),
    (4, 2),
    (6, 3),
    (10, 4),
];

/// Deterministic pseudo-random bytes (xorshift64*), so tests need no
/// external dependency and failures reproduce.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

/// Build k data shards of `len` bytes each, plus m zeroed parity shards,
/// and encode. Returns all k+m shards.
fn encode(k: usize, m: usize, len: usize, seed: u64) -> Vec<Vec<u8>> {
    let rs = ReedSolomon::new(k, m).expect("valid scheme");
    let mut shards: Vec<Vec<u8>> = (0..k)
        .map(|i| pseudo_random(len, seed ^ (i as u64 + 1)))
        .collect();
    shards.extend((0..m).map(|_| vec![0u8; len]));
    rs.encode(&mut shards).expect("encode");
    shards
}

/// Every subset of `n` indices of size `size`, in lexicographic order.
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
fn encode_is_systematic_data_shards_unchanged() {
    for &(k, m) in SCHEMES {
        let len = 4096;
        let originals: Vec<Vec<u8>> = (0..k).map(|i| pseudo_random(len, 7 ^ (i as u64 + 1))).collect();
        let shards = encode(k, m, len, 7);
        for i in 0..k {
            assert_eq!(shards[i], originals[i], "scheme {k}+{m}: data shard {i} was modified by encode");
        }
        assert_eq!(shards.len(), k + m);
    }
}

#[test]
fn verify_accepts_freshly_encoded_shards() {
    for &(k, m) in SCHEMES {
        let rs = ReedSolomon::new(k, m).unwrap();
        let shards = encode(k, m, 1024, 11);
        assert!(rs.verify(&shards).unwrap(), "scheme {k}+{m}");
    }
}

#[test]
fn every_erasure_pattern_up_to_m_reconstructs_exactly() {
    for &(k, m) in SCHEMES {
        let rs = ReedSolomon::new(k, m).unwrap();
        let len = 512;
        let original = encode(k, m, len, 23);
        let n = k + m;
        for lost in 1..=m {
            for pattern in combinations(n, lost) {
                let mut damaged: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
                for &i in &pattern {
                    damaged[i] = None;
                }
                rs.reconstruct(&mut damaged)
                    .unwrap_or_else(|e| panic!("scheme {k}+{m}, erased {pattern:?}: {e:?}"));
                for i in 0..n {
                    assert_eq!(
                        damaged[i].as_ref().unwrap(),
                        &original[i],
                        "scheme {k}+{m}, erased {pattern:?}: shard {i} differs after reconstruct"
                    );
                }
            }
        }
    }
}

#[test]
fn reconstruct_data_only_recovers_data_shards() {
    // The read path (11.3) only needs data shards; reconstruct_data should
    // recover those without also rebuilding parity.
    let (k, m) = (4, 2);
    let rs = ReedSolomon::new(k, m).unwrap();
    let original = encode(k, m, 256, 31);
    let mut damaged: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
    damaged[1] = None;
    damaged[5] = None; // a parity shard; should stay None
    rs.reconstruct_data(&mut damaged).unwrap();
    for i in 0..k {
        assert_eq!(damaged[i].as_ref().unwrap(), &original[i]);
    }
    assert!(damaged[5].is_none(), "reconstruct_data should not rebuild parity");
}

#[test]
fn more_than_m_erasures_is_refused_not_silently_wrong() {
    for &(k, m) in SCHEMES {
        let rs = ReedSolomon::new(k, m).unwrap();
        let original = encode(k, m, 128, 47);
        let mut damaged: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
        for i in 0..=m {
            damaged[i] = None;
        }
        assert_eq!(rs.reconstruct(&mut damaged), Err(Error::TooFewShardsPresent), "scheme {k}+{m}");
    }
}

#[test]
fn undetected_corruption_is_not_caught_by_reconstruct() {
    // This documents the property that motivates per-block checksums
    // (8.3.4, 8.3.5): the code corrects erasures at *known* positions. A
    // corrupt shard that is still marked present is trusted, so the
    // reconstruction of a *different* missing shard comes out wrong.
    let (k, m) = (4, 2);
    let rs = ReedSolomon::new(k, m).unwrap();
    let original = encode(k, m, 256, 59);

    let mut damaged: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
    damaged[0].as_mut().unwrap()[100] ^= 0x01; // one flipped bit, unmarked
    damaged[2] = None; // a genuine erasure elsewhere

    // verify() does notice, because it recomputes parity from all data.
    let present: Vec<Vec<u8>> = {
        let mut p = original.clone();
        p[0][100] ^= 0x01;
        p
    };
    assert!(!rs.verify(&present).unwrap(), "verify should detect the flipped bit");

    // But reconstruct trusts shard 0 and rebuilds shard 2 wrongly.
    rs.reconstruct(&mut damaged).unwrap();
    assert_ne!(
        damaged[2].as_ref().unwrap(),
        &original[2],
        "reconstruct produced correct output despite an unmarked corrupt shard; unexpected"
    );

    // Marking the corrupt shard as an erasure (what a checksum failure
    // does, 8.3.4) recovers both correctly.
    let mut marked: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
    marked[0] = None;
    marked[2] = None;
    rs.reconstruct(&mut marked).unwrap();
    for i in 0..k + m {
        assert_eq!(marked[i].as_ref().unwrap(), &original[i]);
    }
}

#[test]
fn encoding_is_bytewise_independent_so_stripes_can_stream() {
    // Parity of a block equals the concatenation of the parity of its
    // halves (8.2.4). This is what lets the coordinator encode one stripe
    // at a time with bounded memory (3.6).
    for &(k, m) in SCHEMES {
        let rs = ReedSolomon::new(k, m).unwrap();
        let len = 2048;
        let whole = encode(k, m, len, 71);

        let mut first: Vec<Vec<u8>> = whole[..k].iter().map(|s| s[..len / 2].to_vec()).collect();
        first.extend((0..m).map(|_| vec![0u8; len / 2]));
        rs.encode(&mut first).unwrap();

        let mut second: Vec<Vec<u8>> = whole[..k].iter().map(|s| s[len / 2..].to_vec()).collect();
        second.extend((0..m).map(|_| vec![0u8; len / 2]));
        rs.encode(&mut second).unwrap();

        for j in 0..m {
            let mut joined = first[k + j].clone();
            joined.extend_from_slice(&second[k + j]);
            assert_eq!(joined, whole[k + j], "scheme {k}+{m}: parity {j} not bytewise independent");
        }
    }
}

#[test]
fn odd_and_tiny_shard_lengths_work() {
    for &len in &[1usize, 7, 63, 4095] {
        for &(k, m) in SCHEMES {
            let rs = ReedSolomon::new(k, m).unwrap();
            let original = encode(k, m, len, len as u64);
            let mut damaged: Vec<Option<Vec<u8>>> = original.iter().cloned().map(Some).collect();
            damaged[0] = None;
            rs.reconstruct(&mut damaged).unwrap();
            assert_eq!(damaged[0].as_ref().unwrap(), &original[0], "scheme {k}+{m}, len {len}");
        }
    }
}

#[test]
fn parity_rows_are_prefix_stable_across_m() {
    // SPEC 8.1.5: parity shard j of scheme k+m must equal parity shard j
    // of scheme k+m' for every m' > m. Then reducing m is deleting the
    // surplus parity shards (18.9) and increasing m is computing only the
    // new ones. The library builds its matrix as Vandermonde(n, k) times
    // the inverse of the top k rows, so row k+j depends only on j and k,
    // not on n. This test checks the consequence rather than trusting the
    // reasoning.
    for &k in &[1usize, 2, 3, 4, 6, 10] {
        let len = 1024;
        let narrow = encode(k, 1, len, 83 + k as u64);
        for m in 2..=6 {
            let wide = encode(k, m, len, 83 + k as u64);
            assert_eq!(&wide[..k], &narrow[..k], "data differs; test setup error");
            assert_eq!(wide[k], narrow[k], "k={k}: first parity differs between m=1 and m={m}");
            // and every prefix between them
            let mid = encode(k, m - 1, len, 83 + k as u64);
            for j in 0..m - 1 {
                assert_eq!(wide[k + j], mid[k + j], "k={k}: parity {j} differs between m={} and m={m}", m - 1);
            }
        }
    }
}

#[test]
fn first_parity_shard_is_plain_xor_only_for_some_k() {
    // Informational. Appendix A describes single-parity XOR as a property
    // of some matrix constructions. With this library's matrix the first
    // parity row happens to be all ones (plain XOR of the data shards) for
    // k = 1 and k = 3 and for no other k tested. Recorded so that nobody
    // assumes shard k can be checked or rebuilt with a plain XOR in
    // general. If the library changes its matrix this test will say so.
    let len = 256;
    let mut xor_schemes = Vec::new();
    for &k in &[1usize, 2, 3, 4, 6, 10] {
        let shards = encode(k, 1, len, 97);
        let mut xor = vec![0u8; len];
        for s in &shards[..k] {
            for (x, b) in xor.iter_mut().zip(s) {
                *x ^= b;
            }
        }
        if xor == shards[k] {
            xor_schemes.push(k);
        }
    }
    assert_eq!(xor_schemes, vec![1, 3], "set of k for which parity 0 is plain XOR changed (observed value)");
}

#[test]
fn k_equals_one_is_replication() {
    for m in 1..=3 {
        let shards = encode(1, m, 4096, 101);
        for j in 0..m {
            assert_eq!(shards[1 + j], shards[0], "1+{m}: parity shard {j} is not a copy of the data");
        }
    }
}

#[test]
fn m_equals_zero_is_rejected_by_library_and_needs_a_wrapper_special_case() {
    assert_eq!(ReedSolomon::new(3, 0).err(), Some(Error::TooFewParityShards));
}

#[test]
fn shard_count_limit_is_256() {
    // GF(2^8) has 256 elements; the library allows k+m up to 256.
    assert!(ReedSolomon::new(200, 56).is_ok());
    assert_eq!(ReedSolomon::new(200, 57).err(), Some(Error::TooManyShards));
    assert_eq!(ReedSolomon::new(0, 1).err(), Some(Error::TooFewDataShards));
}

#[test]
fn unequal_shard_lengths_are_rejected() {
    let rs = ReedSolomon::new(3, 1).unwrap();
    let mut shards = vec![vec![0u8; 16], vec![0u8; 16], vec![0u8; 15], vec![0u8; 16]];
    assert_eq!(rs.encode(&mut shards), Err(Error::IncorrectShardSize));
}

/// Throughput of encode and of single-shard reconstruction at the block
/// size the specification uses in its worked example. Not a correctness
/// test. Run with `cargo test --release -- --ignored --nocapture`.
#[test]
#[ignore]
fn throughput_at_one_mebibyte_blocks() {
    use std::time::Instant;
    let len = 1 << 20;
    for &(k, m) in &[(3usize, 1usize), (4, 2), (10, 4)] {
        let rs = ReedSolomon::new(k, m).unwrap();
        let mut shards: Vec<Vec<u8>> = (0..k).map(|i| pseudo_random(len, i as u64)).collect();
        shards.extend((0..m).map(|_| vec![0u8; len]));
        let rounds = 20;

        let t = Instant::now();
        for _ in 0..rounds {
            rs.encode(&mut shards).unwrap();
        }
        let enc = t.elapsed();
        let data_bytes = (rounds * k * len) as f64;

        let mut damaged: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        let t = Instant::now();
        for _ in 0..rounds {
            damaged[0] = None;
            rs.reconstruct_data(&mut damaged).unwrap();
        }
        let rec = t.elapsed();

        println!(
            "{k}+{m}: encode {:.0} MiB/s of data, reconstruct one data shard {:.0} MiB/s of data",
            data_bytes / enc.as_secs_f64() / (1 << 20) as f64,
            data_bytes / rec.as_secs_f64() / (1 << 20) as f64,
        );
    }
}
