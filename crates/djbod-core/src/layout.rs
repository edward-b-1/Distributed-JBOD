//! Names under a device root, SPEC 9.1.
//!
//! ```text
//! <device root>/
//!     DISTRIBUTED-JBOD-DEVICE.json   identity file (SPEC 5.2, `device::DEVICE_IDENTITY_FILE`)
//!     objects/
//!         <bucket>/                 v1 has only "default"
//!             ab/cd/<64-hex key hash>/
//!                 <version>.meta.json
//!                 <version>.<shard index>.shard
//! ```

use std::path::{Path, PathBuf};

use crate::erasure::ShardIndex;
use crate::keyhash::KeyHash;
use crate::version::VersionId;

pub const OBJECTS_DIR: &str = "objects";
pub const DEFAULT_BUCKET: &str = "default";
pub const RECORD_SUFFIX: &str = ".meta.json";
pub const SHARD_SUFFIX: &str = ".shard";

/// The directory holding every file of one key within a bucket.
pub fn object_directory(device_root: &Path, bucket: &str, key_hash: &KeyHash) -> PathBuf {
    let [first, second, full] = key_hash.directory_components();
    device_root
        .join(OBJECTS_DIR)
        .join(bucket)
        .join(first)
        .join(second)
        .join(full)
}

/// `<version>.meta.json`
pub fn record_file_name(version: &VersionId) -> String {
    format!("{}{}", version.to_text(), RECORD_SUFFIX)
}

/// `<version>.<shard index>.shard`
pub fn shard_file_name(version: &VersionId, index: ShardIndex) -> String {
    format!("{}.{}{}", version.to_text(), index.0, SHARD_SUFFIX)
}

/// The version and shard index named by a shard file name, if it is one.
pub fn parse_shard_file_name(name: &str) -> Option<(VersionId, ShardIndex)> {
    let stem = name.strip_suffix(SHARD_SUFFIX)?;
    let (version_text, index_text) = stem.split_once('.')?;
    let version = VersionId::from_text(version_text)?;
    let index: u8 = index_text.parse().ok()?;
    Some((version, ShardIndex(index)))
}

/// The version named by a record file name, if it is one.
pub fn parse_record_file_name(name: &str) -> Option<VersionId> {
    let version_text = name.strip_suffix(RECORD_SUFFIX)?;
    VersionId::from_text(version_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyhash::hash_key;

    #[test]
    fn object_directory_follows_the_specification() {
        let hash = hash_key(b"abc"); // ba7816bf...
        let dir = object_directory(Path::new("/mnt/disk0/data"), DEFAULT_BUCKET, &hash);
        assert_eq!(
            dir,
            PathBuf::from(format!(
                "/mnt/disk0/data/objects/default/ba/78/{}",
                hash.to_hex()
            ))
        );
    }

    #[test]
    fn file_names_round_trip() {
        let version = VersionId::from_text("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID");
        let record = record_file_name(&version);
        assert_eq!(record, "01ARZ3NDEKTSV4RRFFQ69G5FAV.meta.json");
        assert_eq!(parse_record_file_name(&record), Some(version));

        let shard = shard_file_name(&version, ShardIndex(7));
        assert_eq!(shard, "01ARZ3NDEKTSV4RRFFQ69G5FAV.7.shard");
        assert_eq!(
            parse_shard_file_name(&shard),
            Some((version, ShardIndex(7)))
        );

        assert_eq!(parse_shard_file_name(&record), None);
        assert_eq!(parse_record_file_name(&shard), None);
        assert_eq!(parse_shard_file_name("junk.shard"), None);
        assert_eq!(
            parse_shard_file_name("01ARZ3NDEKTSV4RRFFQ69G5FAV.x.shard"),
            None
        );
    }
}
