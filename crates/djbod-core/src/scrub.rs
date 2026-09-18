//! The local scrub engine (SPEC 20.1): read everything on one device and
//! check it against the checksums stored beside it, with no network and
//! no coordination. It reports; it does not repair. It is the piece that
//! runs where the disks are; the cluster-wide scrub of 20.1.2 drives it on
//! every node and repairs from the merged findings.
//!
//! The scrubber reads the on-disk format directly and can run while the
//! node is running: shard files and records are immutable once renamed
//! into place, temporaries carry a suffix and are skipped, and a file that
//! vanishes mid-scrub (a concurrent delete) is not reported as damage.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::checksum::block_matches_checksum;
use crate::device::{read_dir_sorted, Device, DeviceError, TEMPORARY_SUFFIX};
use crate::erasure::ShardIndex;
use crate::keyhash::KeyHash;
use crate::layout::{parse_record_file_name, parse_shard_file_name, record_file_name};
use crate::record::{DeviceId, MetadataRecord};
use crate::shardfile::{shard_geometry, ShardFileReader};
use crate::version::VersionId;

#[derive(Debug, Clone)]
pub struct ScrubOptions {
    /// Cap on bytes read per second, so a scrub does not starve clients.
    pub max_bytes_per_second: Option<u64>,
    /// Temporaries older than this are reported (the node deletes them at
    /// its next start, 10.11).
    pub temporary_max_age: Duration,
}

impl Default for ScrubOptions {
    fn default() -> ScrubOptions {
        ScrubOptions {
            max_bytes_per_second: None,
            temporary_max_age: Duration::from_secs(3600),
        }
    }
}

/// One thing wrong on a device. Every variant names the path, and where
/// a record was readable, the key, so an administrator can act and a
/// repair can be driven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// The record failed to parse, failed its own checksum, or failed
    /// validation, or names a key that does not hash to its directory.
    RecordCorrupt { path: PathBuf, reason: String },
    /// The shard file could not be opened: missing header, bad trailer,
    /// checksum, or geometry.
    ShardUnreadable {
        path: PathBuf,
        key: Option<String>,
        version: VersionId,
        shard_index: u8,
        reason: String,
    },
    /// The shard file's header disagrees with where it is or with the
    /// record: wrong key hash, version, index, scheme, or block length.
    ShardMisplaced {
        path: PathBuf,
        key: Option<String>,
        reason: String,
    },
    /// Blocks that failed their checksums.
    ShardBlocksCorrupt {
        path: PathBuf,
        key: Option<String>,
        version: VersionId,
        shard_index: u8,
        stripes: Vec<u64>,
    },
    /// A shard file with no record for its version on this device.
    ShardWithoutRecord {
        path: PathBuf,
        version: VersionId,
        shard_index: u8,
    },
    /// A record on this device whose shard file is not here.
    RecordWithoutShard {
        path: PathBuf,
        key: String,
        version: VersionId,
        shard_index: u8,
    },
    /// A record that does not list this device as a holder at all.
    RecordNotForThisDevice {
        path: PathBuf,
        key: String,
        version: VersionId,
    },
    /// A temporary file older than the configured age.
    StaleTemporary { path: PathBuf, age_secs: u64 },
}

impl Finding {
    /// The key to repair, if the finding is about a version whose record
    /// could be read.
    pub fn repair_key(&self) -> Option<&str> {
        match self {
            Finding::ShardUnreadable { key, .. }
            | Finding::ShardMisplaced { key, .. }
            | Finding::ShardBlocksCorrupt { key, .. } => key.as_deref(),
            Finding::RecordWithoutShard { key, .. } => Some(key),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScrubSummary {
    pub device: Option<DeviceId>,
    pub records_checked: u64,
    pub shards_checked: u64,
    pub blocks_checked: u64,
    pub bytes_read: u64,
    pub findings: Vec<Finding>,
}

/// Paces reads to a byte rate.
struct RateLimiter {
    max_bytes_per_second: Option<u64>,
    started: Instant,
    bytes: u64,
}

impl RateLimiter {
    fn new(max_bytes_per_second: Option<u64>) -> RateLimiter {
        RateLimiter {
            max_bytes_per_second,
            started: Instant::now(),
            bytes: 0,
        }
    }

    fn account(&mut self, bytes: u64) {
        self.bytes += bytes;
        let Some(rate) = self.max_bytes_per_second else {
            return;
        };
        if rate == 0 {
            return;
        }
        let allowed = Duration::from_secs_f64(self.bytes as f64 / rate as f64);
        let elapsed = self.started.elapsed();
        if allowed > elapsed {
            std::thread::sleep(allowed - elapsed);
        }
    }
}

/// Scrub one device. `on_finding` is called as each finding is made so a
/// caller can print progressively; the summary also collects them.
pub fn scrub_device(
    device: &Device,
    options: &ScrubOptions,
    on_finding: &mut dyn FnMut(&Finding),
) -> Result<ScrubSummary, DeviceError> {
    let mut summary = ScrubSummary {
        device: Some(device.id()),
        ..ScrubSummary::default()
    };
    let mut limiter = RateLimiter::new(options.max_bytes_per_second);
    let now = SystemTime::now();

    for key_dir in device.key_directories()? {
        let expected_hash = key_dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(KeyHash::from_hex);
        let mut records: Vec<(VersionId, PathBuf, Option<MetadataRecord>)> = Vec::new();
        let mut shards: Vec<(VersionId, ShardIndex, PathBuf)> = Vec::new();

        for entry in read_dir_sorted(&key_dir)? {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with(TEMPORARY_SUFFIX) {
                if let Ok(modified) = fs::metadata(&entry).and_then(|m| m.modified()) {
                    let age = now.duration_since(modified).unwrap_or_default();
                    if age > options.temporary_max_age {
                        report(
                            &mut summary,
                            on_finding,
                            Finding::StaleTemporary {
                                path: entry.clone(),
                                age_secs: age.as_secs(),
                            },
                        );
                    }
                }
                continue;
            }
            if let Some(version) = parse_record_file_name(&name) {
                summary.records_checked += 1;
                let record = check_record(
                    &entry,
                    expected_hash,
                    device.id(),
                    &mut summary,
                    on_finding,
                    &mut limiter,
                );
                records.push((version, entry.clone(), record));
                continue;
            }
            if let Some((version, index)) = parse_shard_file_name(&name) {
                shards.push((version, index, entry.clone()));
            }
        }

        // Every record here must have its shard here, and vice versa.
        for (version, record_path, record) in &records {
            let Some(record) = record else {
                continue;
            };
            let Some(index) = record.shard_on(device.id()) else {
                report(
                    &mut summary,
                    on_finding,
                    Finding::RecordNotForThisDevice {
                        path: record_path.clone(),
                        key: record.key.clone(),
                        version: *version,
                    },
                );
                continue;
            };
            let present = shards.iter().any(|(v, i, _)| v == version && *i == index);
            if !present && record.size > 0 {
                report(
                    &mut summary,
                    on_finding,
                    Finding::RecordWithoutShard {
                        path: record_path.clone(),
                        key: record.key.clone(),
                        version: *version,
                        shard_index: index.0,
                    },
                );
            }
        }
        for (version, index, shard_path) in &shards {
            let record = records
                .iter()
                .find(|(v, _, _)| v == version)
                .and_then(|(_, _, r)| r.as_ref());
            match record {
                None if !records.iter().any(|(v, _, _)| v == version) => {
                    report(
                        &mut summary,
                        on_finding,
                        Finding::ShardWithoutRecord {
                            path: shard_path.clone(),
                            version: *version,
                            shard_index: index.0,
                        },
                    );
                }
                // Record present but corrupt: already reported; still
                // check the shard on its own terms.
                _ => {}
            }
            summary.shards_checked += 1;
            check_shard(
                shard_path,
                *version,
                *index,
                expected_hash,
                record,
                &mut summary,
                on_finding,
                &mut limiter,
            );
        }
    }
    Ok(summary)
}

fn report(summary: &mut ScrubSummary, on_finding: &mut dyn FnMut(&Finding), finding: Finding) {
    on_finding(&finding);
    summary.findings.push(finding);
}

fn check_record(
    path: &Path,
    expected_hash: Option<KeyHash>,
    device: DeviceId,
    summary: &mut ScrubSummary,
    on_finding: &mut dyn FnMut(&Finding),
    limiter: &mut RateLimiter,
) -> Option<MetadataRecord> {
    let _ = device;
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None, // deleted meanwhile
        Err(e) => {
            report(
                summary,
                on_finding,
                Finding::RecordCorrupt {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                },
            );
            return None;
        }
    };
    limiter.account(json.len() as u64);
    summary.bytes_read += json.len() as u64;
    let record = match MetadataRecord::from_json(&json) {
        Ok(record) => record,
        Err(e) => {
            report(
                summary,
                on_finding,
                Finding::RecordCorrupt {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                },
            );
            return None;
        }
    };
    if expected_hash.is_some() && Some(record.key_hash) != expected_hash {
        report(
            summary,
            on_finding,
            Finding::RecordCorrupt {
                path: path.to_path_buf(),
                reason: format!(
                    "record for key {:?} is in the directory of a different key hash",
                    record.key
                ),
            },
        );
        return None;
    }
    Some(record)
}

#[allow(clippy::too_many_arguments)]
fn check_shard(
    path: &Path,
    version: VersionId,
    index: ShardIndex,
    expected_hash: Option<KeyHash>,
    record: Option<&MetadataRecord>,
    summary: &mut ScrubSummary,
    on_finding: &mut dyn FnMut(&Finding),
    limiter: &mut RateLimiter,
) {
    let key = record.map(|r| r.key.clone());
    let reader = match ShardFileReader::open(path) {
        Ok(reader) => reader,
        Err(crate::shardfile::ShardFileError::Io(e))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            return; // deleted meanwhile
        }
        Err(e) => {
            report(
                summary,
                on_finding,
                Finding::ShardUnreadable {
                    path: path.to_path_buf(),
                    key,
                    version,
                    shard_index: index.0,
                    reason: e.to_string(),
                },
            );
            return;
        }
    };
    let header = reader.header();
    summary.bytes_read += 4096 + reader.footer().encoded_length();
    limiter.account(4096);

    // The file must be what its name and directory say it is.
    let mut mismatches = Vec::new();
    if header.version_id != version {
        mismatches.push(format!(
            "header version {} but file name says {version}",
            header.version_id
        ));
    }
    if header.shard_index != index {
        mismatches.push(format!(
            "header {} but file name says {index}",
            header.shard_index
        ));
    }
    if let Some(expected) = expected_hash {
        if header.key_hash != expected {
            mismatches.push("header key hash differs from the directory".to_string());
        }
    }
    if let Some(record) = record {
        if header.scheme.data_shards() as u8 != record.k
            || header.scheme.parity_shards() as u8 != record.m
        {
            mismatches.push(format!(
                "header scheme {}+{} but record says {}+{}",
                header.scheme.data_shards(),
                header.scheme.parity_shards(),
                record.k,
                record.m
            ));
        }
        if header.block_length != record.block_size {
            mismatches.push(format!(
                "header block length {} but record says {}",
                header.block_length, record.block_size
            ));
        }
        if reader.footer().object_size != record.size {
            mismatches.push(format!(
                "footer object size {} but record says {}",
                reader.footer().object_size,
                record.size
            ));
        }
        if reader.footer().object_checksum != record.object_checksum {
            mismatches.push("footer object checksum differs from the record".to_string());
        }
        if let Some(geometry) = shard_geometry(header.scheme, header.block_length, record.size) {
            if geometry.block_count != reader.block_count() {
                mismatches.push(format!(
                    "{} blocks but the record's size implies {}",
                    reader.block_count(),
                    geometry.block_count
                ));
            }
        }
    }
    if !mismatches.is_empty() {
        report(
            summary,
            on_finding,
            Finding::ShardMisplaced {
                path: path.to_path_buf(),
                key: key.clone(),
                reason: mismatches.join("; "),
            },
        );
    }

    // Every block against its checksum.
    let mut bad_stripes = Vec::new();
    for block_number in 0..reader.block_count() {
        match reader.read_block(block_number) {
            Ok(block) => {
                summary.blocks_checked += 1;
                summary.bytes_read += block.bytes.len() as u64;
                limiter.account(block.bytes.len() as u64);
                if !block_matches_checksum(&block.bytes, block.checksum) {
                    bad_stripes.push(block_number);
                }
            }
            Err(e) => {
                report(
                    summary,
                    on_finding,
                    Finding::ShardUnreadable {
                        path: path.to_path_buf(),
                        key,
                        version,
                        shard_index: index.0,
                        reason: format!("block {block_number}: {e}"),
                    },
                );
                return;
            }
        }
    }
    if !bad_stripes.is_empty() {
        report(
            summary,
            on_finding,
            Finding::ShardBlocksCorrupt {
                path: path.to_path_buf(),
                key,
                version,
                shard_index: index.0,
                stripes: bad_stripes,
            },
        );
    }
}

/// The record file path for a version in a key directory; used by tests
/// and tools that want to name the file a finding refers to.
pub fn record_path(key_dir: &Path, version: &VersionId) -> PathBuf {
    key_dir.join(record_file_name(version))
}
