//! Reed-Solomon erasure coding with typed shard indices, SPEC 8.1.
//!
//! This module is the only place that calls the `reed-solomon-erasure`
//! library, whose interface is positional: the first k entries of a slice
//! are data shards and the next m are parity shards. That rule is applied
//! here and nowhere else. Callers speak in terms of [`ShardIndex`].
//!
//! The library refuses `m = 0`, so replication-free JBOD mode (8.1.2) is
//! handled here without calling it.

use reed_solomon_erasure::galois_8::ReedSolomon;
use thiserror::Error;

/// Limits from SPEC 6.2.4. Generous; they exist to reject typos.
pub const MAX_DATA_SHARDS: u8 = 32;
pub const MAX_PARITY_SHARDS: u8 = 8;
pub const MAX_TOTAL_SHARDS: u8 = 64;

/// The position of a shard within a stripe: `0 .. k-1` are data shards,
/// `k .. k+m-1` are parity shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardIndex(pub u8);

impl ShardIndex {
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for ShardIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shard {}", self.0)
    }
}

/// A validated erasure coding scheme: k data shards and m parity shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    data_shards: u8,
    parity_shards: u8,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemeError {
    #[error("k must be at least 1")]
    NoDataShards,
    #[error("k = {0} exceeds the maximum of {MAX_DATA_SHARDS}")]
    TooManyDataShards(u8),
    #[error("m = {0} exceeds the maximum of {MAX_PARITY_SHARDS}")]
    TooManyParityShards(u8),
    #[error("k + m = {0} exceeds the maximum of {MAX_TOTAL_SHARDS}")]
    TooManyShards(u16),
}

impl Scheme {
    pub fn new(data_shards: u8, parity_shards: u8) -> Result<Scheme, SchemeError> {
        if data_shards == 0 {
            return Err(SchemeError::NoDataShards);
        }
        if data_shards > MAX_DATA_SHARDS {
            return Err(SchemeError::TooManyDataShards(data_shards));
        }
        if parity_shards > MAX_PARITY_SHARDS {
            return Err(SchemeError::TooManyParityShards(parity_shards));
        }
        let total = data_shards as u16 + parity_shards as u16;
        if total > MAX_TOTAL_SHARDS as u16 {
            return Err(SchemeError::TooManyShards(total));
        }
        Ok(Scheme {
            data_shards,
            parity_shards,
        })
    }

    pub fn data_shards(&self) -> usize {
        self.data_shards as usize
    }

    pub fn parity_shards(&self) -> usize {
        self.parity_shards as usize
    }

    pub fn total_shards(&self) -> usize {
        self.data_shards() + self.parity_shards()
    }

    pub fn contains(&self, index: ShardIndex) -> bool {
        index.as_usize() < self.total_shards()
    }

    pub fn is_data(&self, index: ShardIndex) -> bool {
        index.as_usize() < self.data_shards()
    }

    pub fn is_parity(&self, index: ShardIndex) -> bool {
        self.contains(index) && !self.is_data(index)
    }

    /// All shard indices of the scheme, data first, in order.
    pub fn shard_indices(&self) -> Vec<ShardIndex> {
        let mut indices = Vec::with_capacity(self.total_shards());
        for i in 0..self.total_shards() {
            indices.push(ShardIndex(i as u8));
        }
        indices
    }

    /// The data shard indices `0 .. k-1`, in order.
    pub fn data_shard_indices(&self) -> Vec<ShardIndex> {
        let mut indices = Vec::with_capacity(self.data_shards());
        for i in 0..self.data_shards() {
            indices.push(ShardIndex(i as u8));
        }
        indices
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodingError {
    #[error("expected {expected} data blocks, got {actual}")]
    WrongDataBlockCount { expected: usize, actual: usize },
    #[error("blocks must all have the same length")]
    UnequalBlockLengths,
    #[error("blocks must not be empty")]
    EmptyBlocks,
    #[error("{0} is outside the scheme")]
    IndexOutOfRange(ShardIndex),
    #[error("{0} was given more than once")]
    DuplicateIndex(ShardIndex),
    #[error("{present} shards present but {needed} are needed to reconstruct")]
    TooFewShardsPresent { present: usize, needed: usize },
}

/// The Reed-Solomon code for one scheme: computes parity blocks from data
/// blocks and reconstructs missing blocks from present ones.
///
/// This wraps the library's `ReedSolomon` object, which holds the
/// precomputed encoding matrix for the scheme's k and m, and adds three
/// things: shard indices instead of slice positions, validation of inputs
/// with typed errors, and the `m = 0` case the library refuses.
pub struct ReedSolomonCode {
    scheme: Scheme,
    /// `None` when the scheme has no parity shards; the library does not
    /// support that case and there is nothing to compute.
    reed_solomon: Option<ReedSolomon>,
}

impl ReedSolomonCode {
    pub fn new(scheme: Scheme) -> ReedSolomonCode {
        let reed_solomon = if scheme.parity_shards() == 0 {
            None
        } else {
            let rs = ReedSolomon::new(scheme.data_shards(), scheme.parity_shards())
                .expect("failed to initialize ReedSolomon");
            Some(rs)
        };
        ReedSolomonCode {
            scheme,
            reed_solomon,
        }
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Compute the m parity blocks for exactly k data blocks of equal,
    /// non-zero length. Returns them in shard index order `k .. k+m-1`.
    pub fn compute_parity(&self, data_blocks: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, CodingError> {
        if data_blocks.len() != self.scheme.data_shards() {
            return Err(CodingError::WrongDataBlockCount {
                expected: self.scheme.data_shards(),
                actual: data_blocks.len(),
            });
        }
        let block_len = data_blocks[0].len();
        if block_len == 0 {
            return Err(CodingError::EmptyBlocks);
        }
        for block in data_blocks {
            if block.len() != block_len {
                return Err(CodingError::UnequalBlockLengths);
            }
        }

        let mut parity_blocks = Vec::with_capacity(self.scheme.parity_shards());
        for _ in 0..self.scheme.parity_shards() {
            parity_blocks.push(vec![0u8; block_len]);
        }
        if let Some(rs) = &self.reed_solomon {
            rs.encode_sep(data_blocks, &mut parity_blocks)
                .expect("failed to encode parity; inputs were validated");
        }
        Ok(parity_blocks)
    }

    /// Produce the `wanted` blocks from the `present` ones. Present blocks
    /// are returned as copies; absent ones are reconstructed, which needs
    /// at least k present blocks of equal length. The result is in the
    /// order of `wanted`.
    pub fn reconstruct(
        &self,
        present: &[(ShardIndex, &[u8])],
        wanted: &[ShardIndex],
    ) -> Result<Vec<Vec<u8>>, CodingError> {
        let total = self.scheme.total_shards();

        // Lay the present blocks out positionally, checking indices.
        let mut slots: Vec<Option<Vec<u8>>> = vec![None; total];
        let mut block_len: Option<usize> = None;
        for (index, bytes) in present {
            if !self.scheme.contains(*index) {
                return Err(CodingError::IndexOutOfRange(*index));
            }
            if slots[index.as_usize()].is_some() {
                return Err(CodingError::DuplicateIndex(*index));
            }
            match block_len {
                None => block_len = Some(bytes.len()),
                Some(len) if len != bytes.len() => return Err(CodingError::UnequalBlockLengths),
                Some(_) => {}
            }
            slots[index.as_usize()] = Some(bytes.to_vec());
        }
        for index in wanted {
            if !self.scheme.contains(*index) {
                return Err(CodingError::IndexOutOfRange(*index));
            }
        }

        let mut needs_reconstruction = false;
        for index in wanted {
            if slots[index.as_usize()].is_none() {
                needs_reconstruction = true;
            }
        }

        if needs_reconstruction {
            if present.len() < self.scheme.data_shards() {
                return Err(CodingError::TooFewShardsPresent {
                    present: present.len(),
                    needed: self.scheme.data_shards(),
                });
            }
            if block_len == Some(0) {
                return Err(CodingError::EmptyBlocks);
            }
            let rs = self.reed_solomon.as_ref().expect(
                "a scheme with no parity has k total shards, so k present means nothing is missing",
            );

            let mut only_data_wanted = true;
            for index in wanted {
                if self.scheme.is_parity(*index) {
                    only_data_wanted = false;
                }
            }
            if only_data_wanted {
                rs.reconstruct_data(&mut slots)
                    .expect("failed to reconstruct data shards; inputs were validated");
            } else {
                rs.reconstruct(&mut slots)
                    .expect("failed to reconstruct shards; inputs were validated");
            }
        }

        let mut result = Vec::with_capacity(wanted.len());
        for index in wanted {
            let block = slots[index.as_usize()]
                .as_ref()
                .expect("wanted block is present or was reconstructed");
            result.push(block.clone());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_limits_are_enforced() {
        assert_eq!(Scheme::new(0, 1), Err(SchemeError::NoDataShards));
        assert_eq!(Scheme::new(33, 1), Err(SchemeError::TooManyDataShards(33)));
        assert_eq!(Scheme::new(4, 9), Err(SchemeError::TooManyParityShards(9)));
        assert_eq!(
            Scheme::new(32, 8),
            Ok(Scheme {
                data_shards: 32,
                parity_shards: 8
            })
        );
        // k + m limit is only reachable if the individual limits allow it;
        // with 32 and 8 it is not, but the check is kept in case they change.
        assert!(Scheme::new(1, 0).is_ok());
    }

    #[test]
    fn shard_indices_are_classified_by_position() {
        let scheme = Scheme::new(3, 2).expect("valid scheme");
        assert!(scheme.is_data(ShardIndex(0)));
        assert!(scheme.is_data(ShardIndex(2)));
        assert!(scheme.is_parity(ShardIndex(3)));
        assert!(scheme.is_parity(ShardIndex(4)));
        assert!(!scheme.contains(ShardIndex(5)));
        assert!(!scheme.is_parity(ShardIndex(5)));
        assert_eq!(scheme.shard_indices().len(), 5);
        assert_eq!(
            scheme.data_shard_indices(),
            vec![ShardIndex(0), ShardIndex(1), ShardIndex(2)]
        );
    }

    #[test]
    fn compute_parity_validates_its_input() {
        let code = ReedSolomonCode::new(Scheme::new(2, 1).expect("valid scheme"));
        assert_eq!(
            code.compute_parity(&[vec![1u8; 4]]),
            Err(CodingError::WrongDataBlockCount {
                expected: 2,
                actual: 1
            })
        );
        assert_eq!(
            code.compute_parity(&[vec![1u8; 4], vec![1u8; 3]]),
            Err(CodingError::UnequalBlockLengths)
        );
        assert_eq!(
            code.compute_parity(&[vec![], vec![]]),
            Err(CodingError::EmptyBlocks)
        );
    }

    #[test]
    fn no_parity_scheme_computes_nothing_and_reconstructs_nothing() {
        let code = ReedSolomonCode::new(Scheme::new(2, 0).expect("valid scheme"));
        let data = [vec![1u8; 8], vec![2u8; 8]];
        assert_eq!(code.compute_parity(&data), Ok(vec![]));

        let present: [(ShardIndex, &[u8]); 2] =
            [(ShardIndex(0), &data[0]), (ShardIndex(1), &data[1])];
        let out = code
            .reconstruct(&present, &[ShardIndex(1), ShardIndex(0)])
            .expect("everything present");
        assert_eq!(out, vec![data[1].clone(), data[0].clone()]);

        let only_one: [(ShardIndex, &[u8]); 1] = [(ShardIndex(0), &data[0])];
        assert_eq!(
            code.reconstruct(&only_one, &[ShardIndex(1)]),
            Err(CodingError::TooFewShardsPresent {
                present: 1,
                needed: 2
            })
        );
    }

    #[test]
    fn reconstruct_rejects_bad_indices() {
        let code = ReedSolomonCode::new(Scheme::new(2, 1).expect("valid scheme"));
        let block = [0u8; 4];
        let out_of_range: [(ShardIndex, &[u8]); 1] = [(ShardIndex(3), &block)];
        assert_eq!(
            code.reconstruct(&out_of_range, &[ShardIndex(0)]),
            Err(CodingError::IndexOutOfRange(ShardIndex(3)))
        );
        let duplicate: [(ShardIndex, &[u8]); 2] =
            [(ShardIndex(0), &block), (ShardIndex(0), &block)];
        assert_eq!(
            code.reconstruct(&duplicate, &[ShardIndex(1)]),
            Err(CodingError::DuplicateIndex(ShardIndex(0)))
        );
        let fine: [(ShardIndex, &[u8]); 2] = [(ShardIndex(0), &block), (ShardIndex(1), &block)];
        assert_eq!(
            code.reconstruct(&fine, &[ShardIndex(7)]),
            Err(CodingError::IndexOutOfRange(ShardIndex(7)))
        );
    }

    #[test]
    fn any_k_of_total_recover_every_shard() {
        for (k, m) in [
            (1u8, 1u8),
            (1, 2),
            (2, 1),
            (3, 1),
            (3, 2),
            (4, 2),
            (6, 3),
            (10, 4),
        ] {
            let scheme = Scheme::new(k, m).expect("valid scheme");
            let code = ReedSolomonCode::new(scheme);
            let block_len = 64;

            let mut data = Vec::with_capacity(scheme.data_shards());
            for i in 0..scheme.data_shards() {
                data.push(vec![(i as u8).wrapping_mul(37).wrapping_add(k); block_len]);
            }
            let parity = code
                .compute_parity(&data)
                .expect("failed to compute parity");
            let mut all = data.clone();
            all.extend(parity);

            // Drop the first m shards (a mix of data and parity for m >= k)
            // and ask for everything back.
            let mut present: Vec<(ShardIndex, &[u8])> = Vec::new();
            for i in (m as usize)..scheme.total_shards() {
                present.push((ShardIndex(i as u8), &all[i]));
            }
            let out = code
                .reconstruct(&present, &scheme.shard_indices())
                .expect("failed to reconstruct shards");
            assert_eq!(out, all, "scheme {k}+{m}");
        }
    }
}
