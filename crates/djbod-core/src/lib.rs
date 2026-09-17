//! Core library for Distributed-JBOD: on-disk format, checksums, and
//! erasure coding. No networking. See SPEC.md, Appendix C.
//!
//! Layers, lowest first:
//!
//! - [`erasure`]: Reed-Solomon coding with typed shard indices. Given
//!   blocks that are known to be missing, recovers them. Knows nothing
//!   about checksums.
//! - [`checksum`]: the 64-bit block checksum (SPEC 8.3). Knows nothing
//!   about erasure coding.
//! - [`stripe`]: encodes a stripe of object bytes into checksummed blocks,
//!   and decodes received blocks by verifying every checksum, treating each
//!   mismatch or absence as an erasure, and reconstructing (SPEC 8.2, 8.3.4).

pub mod checksum;
pub mod erasure;
pub mod stripe;
