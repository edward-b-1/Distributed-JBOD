//! The metadata record, SPEC 9.4: one JSON document per version, stored
//! in full on every device that holds a shard of that version (4.3).
//!
//! The record is the only thing that knows where a version's shards are
//! (4.2). It is human-readable on purpose (9.1.4, 9.4.1): a disk can be
//! searched for an object by key with ordinary tools. It also records the
//! encoding parameters the version was written with (6.3), so a recovery
//! tool needs no cluster configuration and the global values can change
//! without making old objects unreadable.
//!
//! On disk the record carries a `checksum` field beside its other fields
//! (9.4.5). The checksum is XXH3-64 over the *canonical* form of the record
//! without that field, not over the file's bytes, so the file may be
//! pretty-printed and its keys may be in any order. Canonical form: JSON with object keys sorted bytewise, no
//! whitespace, integers in decimal, strings with JSON's minimal escaping,
//! optional fields omitted when absent. Modelled on the JSON
//! Canonicalization Scheme (RFC 8785) for the value types used here, which
//! exclude floats.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::checksum::{checksum_block, BlockChecksum};
use crate::erasure::{Scheme, SchemeError, ShardIndex};
use crate::keyhash::{hash_key, KeyHash};
use crate::version::VersionId;

pub const RECORD_FORMAT_VERSION: u32 = 1;
pub const SYSTEM_NAME: &str = "distributed-jbod";

/// The identity of a device (5.2): a UUID written into
/// `DISTRIBUTED-JBOD-DEVICE.json` on first use, and the device's name in
/// every record. Never a path, and never a node: a disk moved to another
/// machine keeps its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub Uuid);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device {}", self.0)
    }
}

/// Where one shard of a version lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardLocation {
    pub index: u8,
    pub device: DeviceId,
}

/// One version of one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataRecord {
    pub format_version: u32,
    pub system: String,
    pub bucket: String,
    pub key: String,
    pub key_hash: KeyHash,
    pub version: VersionId,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    /// True object length in bytes, before padding.
    pub size: u64,
    /// XXH3-64 of the whole object (8.3.6).
    pub object_checksum: BlockChecksum,
    /// The global values when this version was written (6.3).
    pub k: u8,
    pub m: u8,
    pub block_size: u64,
    /// Exactly k + m entries, one per shard index, each on a distinct
    /// device (7.2).
    pub shards: Vec<ShardLocation>,
    /// Placement revision (SPEC 18.8.1): 0 when the version is first
    /// written, incremented by every re-placement of a shard. The version
    /// id identifies the body; the revision identifies where it lives.
    /// Omitted from the JSON when 0, so records written before the field
    /// existed still verify.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Opaque to the native layer; reserved for clients and the future
    /// translation layer (19.2.3).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub user_metadata: BTreeMap<String, String>,
}

/// Longest content type a record may carry, in bytes (SPEC 9.4.2). The
/// user metadata map has no fixed bound here; the cluster document sets
/// one (`max_user_metadata_bytes`) that the coordinator enforces on write.
pub const MAX_CONTENT_TYPE_BYTES: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("not valid JSON or not a metadata record: {0}")]
    Json(String),
    #[error("content type is {0} bytes; the limit is {MAX_CONTENT_TYPE_BYTES}")]
    ContentTypeTooLong(usize),
    #[error(
        "record checksum {stored:?} does not match its contents, which checksum to {computed:?}"
    )]
    ChecksumMismatch {
        stored: BlockChecksum,
        computed: BlockChecksum,
    },
    #[error("system field is {0:?}, not {SYSTEM_NAME:?}")]
    WrongSystem(String),
    #[error("unsupported record format version {0}")]
    UnsupportedFormatVersion(u32),
    #[error("key hash {stored:?} does not match the key, whose hash is {computed:?}")]
    KeyHashMismatch { stored: KeyHash, computed: KeyHash },
    #[error("invalid scheme: {0}")]
    InvalidScheme(#[from] SchemeError),
    #[error("block size {0} is not a positive multiple of 4096")]
    BadBlockSize(u64),
    #[error("record lists {actual} shards but the scheme has {expected}")]
    WrongShardCount { expected: usize, actual: usize },
    #[error("shard index {0} is missing from the record")]
    MissingShardIndex(u8),
    #[error("shard index {0} appears more than once")]
    DuplicateShardIndex(u8),
    #[error("device {0:?} holds more than one shard")]
    DuplicateDevice(DeviceId),
}

/// The name of the on-disk field holding the record checksum. It is not
/// a field of `MetadataRecord`: it describes the file, not the version.
pub const CHECKSUM_FIELD: &str = "checksum";

/// Write a JSON value in canonical form: keys sorted bytewise, no
/// whitespace. Numbers and strings are written as serde_json writes them,
/// which for the integers and strings in a record is the canonical
/// spelling.
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a string always serializes"));
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl MetadataRecord {
    /// True if `other` describes the same body as this record: same
    /// version, key, size, checksum, and scheme. Two records that agree on
    /// this may differ only in placement (`shards`) and `revision`.
    pub fn same_body(&self, other: &MetadataRecord) -> bool {
        self.version == other.version
            && self.key == other.key
            && self.key_hash == other.key_hash
            && self.size == other.size
            && self.object_checksum == other.object_checksum
            && self.k == other.k
            && self.m == other.m
            && self.block_size == other.block_size
    }

    /// The canonical form the record checksum covers (9.4.5).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).expect("a metadata record always serializes");
        let mut out = String::new();
        write_canonical(&value, &mut out);
        out.into_bytes()
    }

    /// XXH3-64 over the canonical form.
    pub fn checksum(&self) -> BlockChecksum {
        checksum_block(&self.canonical_bytes())
    }

    /// Serialize the on-disk form as indented JSON, for humans as much as
    /// for machines: the record's fields plus a `checksum` field computed
    /// here over the rest.
    pub fn to_json(&self) -> String {
        // Serialize the struct directly so the file keeps the field order
        // declared above (going through a JSON value would sort the keys),
        // then append the checksum as the last field.
        let mut json =
            serde_json::to_string_pretty(self).expect("a metadata record always serializes");
        let closing_brace = json
            .trim_end()
            .strip_suffix('}')
            .expect("a struct serializes to an object")
            .len();
        json.truncate(closing_brace);
        json.push_str(&format!(
            ",\n  \"{CHECKSUM_FIELD}\": \"{}\"\n}}",
            self.checksum().to_hex()
        ));
        json
    }

    /// Parse the on-disk form, verify its checksum, and validate.
    pub fn from_json(json: &str) -> Result<MetadataRecord, RecordError> {
        let mut value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| RecordError::Json(e.to_string()))?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| RecordError::Json("record is not a JSON object".to_string()))?;
        let stored = match fields.remove(CHECKSUM_FIELD) {
            Some(serde_json::Value::String(hex)) => {
                BlockChecksum::from_hex(&hex).ok_or_else(|| {
                    RecordError::Json(format!("checksum {hex:?} is not 16 hex characters"))
                })?
            }
            Some(_) => return Err(RecordError::Json("checksum is not a string".to_string())),
            None => {
                return Err(RecordError::Json(
                    "record has no checksum field".to_string(),
                ))
            }
        };
        let record: MetadataRecord =
            serde_json::from_value(value).map_err(|e| RecordError::Json(e.to_string()))?;
        let computed = record.checksum();
        if computed != stored {
            return Err(RecordError::ChecksumMismatch { stored, computed });
        }
        record.validate()?;
        Ok(record)
    }

    /// Check the invariants a record must satisfy regardless of where it
    /// came from.
    /// The size of the user metadata map: the sum of its keys' and
    /// values' lengths in bytes.
    pub fn user_metadata_bytes(&self) -> usize {
        self.user_metadata
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum()
    }

    pub fn validate(&self) -> Result<(), RecordError> {
        if self.system != SYSTEM_NAME {
            return Err(RecordError::WrongSystem(self.system.clone()));
        }
        if let Some(content_type) = &self.content_type {
            if content_type.len() > MAX_CONTENT_TYPE_BYTES {
                return Err(RecordError::ContentTypeTooLong(content_type.len()));
            }
        }
        if self.format_version != RECORD_FORMAT_VERSION {
            return Err(RecordError::UnsupportedFormatVersion(self.format_version));
        }
        let computed = hash_key(self.key.as_bytes());
        if computed != self.key_hash {
            return Err(RecordError::KeyHashMismatch {
                stored: self.key_hash,
                computed,
            });
        }
        let scheme = Scheme::new(self.k, self.m)?;
        if self.block_size == 0 || !self.block_size.is_multiple_of(4096) {
            return Err(RecordError::BadBlockSize(self.block_size));
        }
        if self.shards.len() != scheme.total_shards() {
            return Err(RecordError::WrongShardCount {
                expected: scheme.total_shards(),
                actual: self.shards.len(),
            });
        }
        let mut seen_index = vec![false; scheme.total_shards()];
        let mut seen_devices: Vec<DeviceId> = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            if !scheme.contains(ShardIndex(shard.index)) {
                return Err(RecordError::MissingShardIndex(shard.index));
            }
            if seen_index[shard.index as usize] {
                return Err(RecordError::DuplicateShardIndex(shard.index));
            }
            seen_index[shard.index as usize] = true;
            if seen_devices.contains(&shard.device) {
                return Err(RecordError::DuplicateDevice(shard.device));
            }
            seen_devices.push(shard.device);
        }
        for (index, seen) in seen_index.iter().enumerate() {
            if !seen {
                return Err(RecordError::MissingShardIndex(index as u8));
            }
        }
        Ok(())
    }

    /// The scheme this version was written with. Valid records always
    /// have one.
    pub fn scheme(&self) -> Result<Scheme, SchemeError> {
        Scheme::new(self.k, self.m)
    }

    /// The device holding shard `index`, if the record lists it.
    pub fn device_for(&self, index: ShardIndex) -> Option<DeviceId> {
        for shard in &self.shards {
            if shard.index == index.0 {
                return Some(shard.device);
            }
        }
        None
    }

    /// The shard index held by `device`, if any.
    pub fn shard_on(&self, device: DeviceId) -> Option<ShardIndex> {
        for shard in &self.shards {
            if shard.device == device {
                return Some(ShardIndex(shard.index));
            }
        }
        None
    }
}
