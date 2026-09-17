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

//! - [`keyhash`]: the SHA-256 key hash that names an object's directory
//!   (SPEC 9.1).
//! - [`version`]: the 16-byte version identifier of one body (SPEC 9.2).
//! - [`shardfile`]: the on-disk file holding one shard of one version, with
//!   its header, blocks, footer, and trailer (SPEC 9.3).
//! - [`record`]: the metadata record describing one version, stored as
//!   JSON on every device that holds a shard of it (SPEC 9.4).
//! - [`layout`]: the directory and file names under a device root
//!   (SPEC 9.1).
//! - [`device`]: one device on disk: identity file, atomic writes of shard
//!   files and records, reads, deletes, free space, and cleanup of
//!   temporaries (SPEC 5, 9.3.3, 9.4.3, 10.6, 10.11).

pub mod checksum;
pub mod device;
pub mod erasure;
pub mod keyhash;
pub mod layout;
pub mod record;
pub mod shardfile;
pub mod stripe;
pub mod version;
