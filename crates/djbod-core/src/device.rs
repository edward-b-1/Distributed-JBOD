//! One device on disk: a directory on one filesystem that this system
//! owns (SPEC 5).
//!
//! The device layer is the only code that touches a device's directory
//! tree. It owns the identity file, the temporary-name, fsync, rename
//! procedure for shard files and records (9.3.3, 9.4.3), space
//! reservation (10.6), and cleanup of temporaries (10.11). It is
//! synchronous; the node calls it from blocking worker threads (4.4).

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::checksum::BlockChecksum;
use crate::keyhash::KeyHash;
use crate::layout::{
    object_directory, parse_record_file_name, record_file_name, shard_file_name, DEFAULT_BUCKET,
    OBJECTS_DIR, RECORD_SUFFIX, SHARD_SUFFIX,
};
use crate::record::{DeviceId, MetadataRecord, RecordError, SYSTEM_NAME};
use crate::shardfile::{
    shard_file_length, ShardFileError, ShardFileFooter, ShardFileHeader, ShardFileReader,
    ShardFileWriter,
};
use crate::stripe::ShardBlock;
use crate::version::VersionId;

/// The identity file's name (SPEC 5.2). Uppercase so it sorts first and
/// carries the project name so it can be searched for.
pub const DEVICE_IDENTITY_FILE: &str = "DISTRIBUTED-JBOD-DEVICE.json";
pub const DEVICE_FORMAT_VERSION: u32 = 1;
pub const TEMPORARY_SUFFIX: &str = ".tmp";
/// The one entry an otherwise empty directory may contain (5.2.1).
const LOST_AND_FOUND: &str = "lost+found";

const NOTICE: &str = "This directory is a storage device of Distributed-JBOD, a distributed \
object store (https://github.com/edward-b-1/Distributed-JBOD). Its contents are managed by the \
distributed-jbod node process. Do not add, edit, move, or delete files here by hand; doing so \
can destroy data that other machines depend on. To stop using this directory as a device, \
drain and remove it through the cluster's administrative commands.";

/// The contents of the identity file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub system: String,
    pub notice: String,
    pub format_version: u32,
    pub device_id: DeviceId,
    pub cluster_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("I/O error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("{path} is not empty (found {found:?}); a new device needs an empty directory")]
    NotEmpty { path: PathBuf, found: Vec<String> },
    #[error("{path} has no identity file; it is not an initialised device")]
    NotInitialised { path: PathBuf },
    #[error(
        "{path} has an objects directory but no identity file; the identity file was deleted or the directory belongs to something else"
    )]
    ForeignDirectory { path: PathBuf },
    #[error("identity file at {path} is not valid: {reason}")]
    BadIdentity { path: PathBuf, reason: String },
    #[error("device {device:?} at {path} belongs to cluster {actual}, not {expected}")]
    WrongCluster {
        path: PathBuf,
        device: DeviceId,
        expected: Uuid,
        actual: Uuid,
    },
    #[error("shard file error: {0}")]
    ShardFile(#[from] ShardFileError),
    #[error("record error at {path}: {source}")]
    Record { path: PathBuf, source: RecordError },
    #[error("record {version} for key hash {key_hash:?} already exists on this device and the new one is not a higher placement revision of the same body")]
    RecordExists {
        key_hash: KeyHash,
        version: VersionId,
    },
    #[error("object size {object_size} is not valid for this scheme and block length")]
    BadObjectSize { object_size: u64 },
}

fn io_error(path: &Path, source: io::Error) -> DeviceError {
    DeviceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// fsync a directory so that a rename or creation inside it is durable.
fn fsync_directory(dir: &Path) -> Result<(), DeviceError> {
    let handle = File::open(dir).map_err(|e| io_error(dir, e))?;
    handle.sync_all().map_err(|e| io_error(dir, e))
}

/// Write `bytes` to `final_path` atomically: temporary name in the same
/// directory, fsync, rename, fsync the directory.
fn write_file_atomically(final_path: &Path, bytes: &[u8]) -> Result<(), DeviceError> {
    let temp_path = temporary_path(final_path);
    let mut file = File::create(&temp_path).map_err(|e| io_error(&temp_path, e))?;
    if let Err(e) = io::Write::write_all(&mut file, bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(io_error(&temp_path, e));
    }
    drop(file);
    if let Err(e) = fs::rename(&temp_path, final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(io_error(final_path, e));
    }
    let dir = final_path.parent().expect("a file path has a parent");
    fsync_directory(dir)
}

/// Temporary names carry a token unique to this process and call, so two
/// writers that somehow target the same final name can never share a
/// temporary file (SPEC 20.1.2.1). The `.tmp` suffix is what cleanup and
/// the scrubber look for.
fn temporary_path(final_path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = final_path
        .file_name()
        .expect("a file path has a name")
        .to_os_string();
    name.push(format!(
        ".{}-{}{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        TEMPORARY_SUFFIX
    ));
    final_path.with_file_name(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceReport {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// An initialised device: a directory with an identity file.
#[derive(Debug)]
pub struct Device {
    root: PathBuf,
    identity: DeviceIdentity,
    filesystem_id: u64,
}

impl Device {
    /// Make `root` a device of `cluster_id`. The directory must exist and
    /// be empty apart from `lost+found` (5.2.1).
    pub fn initialise(root: &Path, cluster_id: Uuid) -> Result<Device, DeviceError> {
        let metadata = fs::metadata(root).map_err(|e| io_error(root, e))?;
        if !metadata.is_dir() {
            return Err(DeviceError::NotADirectory {
                path: root.to_path_buf(),
            });
        }
        let mut found = Vec::new();
        for entry in fs::read_dir(root).map_err(|e| io_error(root, e))? {
            let entry = entry.map_err(|e| io_error(root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != LOST_AND_FOUND {
                found.push(name);
            }
        }
        if !found.is_empty() {
            found.sort();
            return Err(DeviceError::NotEmpty {
                path: root.to_path_buf(),
                found,
            });
        }

        let identity = DeviceIdentity {
            system: SYSTEM_NAME.to_string(),
            notice: NOTICE.to_string(),
            format_version: DEVICE_FORMAT_VERSION,
            device_id: DeviceId(Uuid::new_v4()),
            cluster_id,
            created: OffsetDateTime::now_utc(),
        };
        let objects = root.join(OBJECTS_DIR).join(DEFAULT_BUCKET);
        fs::create_dir_all(&objects).map_err(|e| io_error(&objects, e))?;
        let json = serde_json::to_string_pretty(&identity).expect("identity always serializes");
        write_file_atomically(&root.join(DEVICE_IDENTITY_FILE), json.as_bytes())?;
        fsync_directory(root)?;

        Ok(Device {
            root: root.to_path_buf(),
            identity,
            filesystem_id: metadata.dev(),
        })
    }

    /// Open an initialised device. If `expected_cluster` is given, the
    /// identity file must name it (5.4).
    pub fn open(root: &Path, expected_cluster: Option<Uuid>) -> Result<Device, DeviceError> {
        let metadata = fs::metadata(root).map_err(|e| io_error(root, e))?;
        if !metadata.is_dir() {
            return Err(DeviceError::NotADirectory {
                path: root.to_path_buf(),
            });
        }
        let identity_path = root.join(DEVICE_IDENTITY_FILE);
        let json = match fs::read_to_string(&identity_path) {
            Ok(json) => json,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if root.join(OBJECTS_DIR).is_dir() {
                    return Err(DeviceError::ForeignDirectory {
                        path: root.to_path_buf(),
                    });
                }
                return Err(DeviceError::NotInitialised {
                    path: root.to_path_buf(),
                });
            }
            Err(e) => return Err(io_error(&identity_path, e)),
        };
        let identity: DeviceIdentity =
            serde_json::from_str(&json).map_err(|e| DeviceError::BadIdentity {
                path: identity_path.clone(),
                reason: e.to_string(),
            })?;
        if identity.system != SYSTEM_NAME {
            return Err(DeviceError::BadIdentity {
                path: identity_path,
                reason: format!("system is {:?}, not {SYSTEM_NAME:?}", identity.system),
            });
        }
        if identity.format_version != DEVICE_FORMAT_VERSION {
            return Err(DeviceError::BadIdentity {
                path: identity_path,
                reason: format!("unsupported format version {}", identity.format_version),
            });
        }
        if let Some(expected) = expected_cluster {
            if identity.cluster_id != expected {
                return Err(DeviceError::WrongCluster {
                    path: root.to_path_buf(),
                    device: identity.device_id,
                    expected,
                    actual: identity.cluster_id,
                });
            }
        }
        Ok(Device {
            root: root.to_path_buf(),
            identity,
            filesystem_id: metadata.dev(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> DeviceId {
        self.identity.device_id
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// The filesystem's identifier (`st_dev`). Two configured devices with
    /// the same value are one disk, which the node refuses (5.3).
    pub fn filesystem_id(&self) -> u64 {
        self.filesystem_id
    }

    /// Bytes available to this process on the filesystem, less `headroom`
    /// as a fraction of the filesystem's total size (5.5). Reservations in
    /// flight are already excluded, because `fallocate` takes them from
    /// the filesystem's free count.
    pub fn free_space(&self, headroom: f64) -> Result<u64, DeviceError> {
        Ok(self.space(headroom)?.free_bytes)
    }

    /// Total size of the filesystem and the bytes this system may still
    /// use on it (5.5).
    pub fn space(&self, headroom: f64) -> Result<SpaceReport, DeviceError> {
        let stat = rustix::fs::statvfs(&self.root)
            .map_err(|errno| io_error(&self.root, io::Error::from(errno)))?;
        let available = stat.f_bavail * stat.f_frsize;
        let total = stat.f_blocks * stat.f_frsize;
        let reserved = (total as f64 * headroom) as u64;
        Ok(SpaceReport {
            total_bytes: total,
            free_bytes: available.saturating_sub(reserved),
        })
    }

    /// Remove the temporary file of a shard write that did not finish,
    /// if one exists. Used by `AbortShard` (19.1.3) when the coordinator
    /// that started the write is cleaning up after a failure.
    pub fn remove_temporary_shard(
        &self,
        key_hash: &KeyHash,
        version: &VersionId,
        index: crate::erasure::ShardIndex,
    ) -> Result<(), DeviceError> {
        let final_path = self
            .object_directory(key_hash)
            .join(shard_file_name(version, index));
        remove_if_present(&temporary_path(&final_path))
    }

    pub fn object_directory(&self, key_hash: &KeyHash) -> PathBuf {
        object_directory(&self.root, DEFAULT_BUCKET, key_hash)
    }

    /// Every key directory on this device, in path order.
    pub fn key_directories(&self) -> Result<Vec<PathBuf>, DeviceError> {
        let bucket = self.root.join(OBJECTS_DIR).join(DEFAULT_BUCKET);
        let mut out = Vec::new();
        for first in read_dir_sorted(&bucket)? {
            for second in read_dir_sorted(&first)? {
                for key_dir in read_dir_sorted(&second)? {
                    if key_dir.is_dir() {
                        out.push(key_dir);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Begin writing one shard file. Creates the object directory if
    /// needed, creates the temporary file, reserves its full length
    /// (10.6), and writes the header. `ENOSPC` surfaces as an I/O error.
    pub fn begin_shard(
        &self,
        key_hash: &KeyHash,
        header: ShardFileHeader,
        object_size: u64,
    ) -> Result<ShardWrite, DeviceError> {
        let length = shard_file_length(header.scheme, header.block_length, object_size)
            .ok_or(DeviceError::BadObjectSize { object_size })?;
        let dir = self.object_directory(key_hash);
        fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        let final_path = dir.join(shard_file_name(&header.version_id, header.shard_index));
        let temp_path = temporary_path(&final_path);
        let writer = ShardFileWriter::create_with_reservation(&temp_path, header, Some(length))?;
        Ok(ShardWrite {
            writer: Some(writer),
            temp_path,
            final_path,
            expected_length: length,
        })
    }

    /// Store a record for a version on this device (9.4.3). An existing
    /// record for the same version is replaced only by one describing the
    /// same body at a higher placement revision (SPEC 18.8.2); anything
    /// else is refused.
    pub fn write_record(&self, record: &MetadataRecord) -> Result<(), DeviceError> {
        let dir = self.object_directory(&record.key_hash);
        record.validate().map_err(|source| DeviceError::Record {
            path: dir.clone(),
            source,
        })?;
        fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        let path = dir.join(record_file_name(&record.version));
        if path.exists() {
            let existing = self.read_record(&record.key_hash, &record.version)?;
            if existing == *record {
                return Ok(()); // idempotent
            }
            if !(existing.same_body(record) && record.revision > existing.revision) {
                return Err(DeviceError::RecordExists {
                    key_hash: record.key_hash,
                    version: record.version,
                });
            }
        }
        write_file_atomically(&path, record.to_json().as_bytes())
    }

    /// Every record under a key hash on this device, oldest version first.
    /// A record that fails to parse or validate is an error (16.1).
    pub fn read_records(&self, key_hash: &KeyHash) -> Result<Vec<MetadataRecord>, DeviceError> {
        let dir = self.object_directory(key_hash);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(&dir, e)),
        };
        let mut versions: Vec<VersionId> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_error(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(version) = parse_record_file_name(&name) {
                versions.push(version);
            }
        }
        versions.sort();
        let mut records = Vec::with_capacity(versions.len());
        for version in versions {
            records.push(self.read_record(key_hash, &version)?);
        }
        Ok(records)
    }

    pub fn read_record(
        &self,
        key_hash: &KeyHash,
        version: &VersionId,
    ) -> Result<MetadataRecord, DeviceError> {
        let path = self
            .object_directory(key_hash)
            .join(record_file_name(version));
        let json = fs::read_to_string(&path).map_err(|e| io_error(&path, e))?;
        let record = MetadataRecord::from_json(&json).map_err(|source| DeviceError::Record {
            path: path.clone(),
            source,
        })?;
        if record.key_hash != *key_hash {
            return Err(DeviceError::Record {
                path,
                source: RecordError::KeyHashMismatch {
                    stored: record.key_hash,
                    computed: *key_hash,
                },
            });
        }
        Ok(record)
    }

    /// Open the shard file this device holds for a version.
    pub fn open_shard(
        &self,
        key_hash: &KeyHash,
        version: &VersionId,
        index: crate::erasure::ShardIndex,
    ) -> Result<ShardFileReader, DeviceError> {
        let path = self
            .object_directory(key_hash)
            .join(shard_file_name(version, index));
        Ok(ShardFileReader::open(&path)?)
    }

    /// Remove a version from this device: record first, then any shard
    /// file, then the key directory if it is now empty (14.2). Removing
    /// what is already absent succeeds.
    pub fn delete_version(
        &self,
        key_hash: &KeyHash,
        version: &VersionId,
    ) -> Result<(), DeviceError> {
        let dir = self.object_directory(key_hash);
        if !dir.exists() {
            return Ok(());
        }
        remove_if_present(&dir.join(record_file_name(version)))?;
        let prefix = format!("{}.", version.to_text());
        for entry in fs::read_dir(&dir).map_err(|e| io_error(&dir, e))? {
            let entry = entry.map_err(|e| io_error(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(SHARD_SUFFIX) {
                remove_if_present(&entry.path())?;
            }
        }
        fsync_directory(&dir)?;
        match fs::remove_dir(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) => {
                Ok(())
            }
            Err(e) => Err(io_error(&dir, e)),
        }
    }

    /// Visit every record on this device, in directory order. Used by
    /// listing (15.1) and by scans that look for records referencing a
    /// device (18.5). Records that fail to parse are reported to `on_bad`
    /// and skipped, so one corrupt record does not hide the rest.
    pub fn walk_records(
        &self,
        mut on_record: impl FnMut(&MetadataRecord),
        mut on_bad: impl FnMut(&Path, &DeviceError),
    ) -> Result<(), DeviceError> {
        let bucket = self.root.join(OBJECTS_DIR).join(DEFAULT_BUCKET);
        for first in read_dir_sorted(&bucket)? {
            for second in read_dir_sorted(&first)? {
                for key_dir in read_dir_sorted(&second)? {
                    for entry in read_dir_sorted(&key_dir)? {
                        let name = entry
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        if !name.ends_with(RECORD_SUFFIX) {
                            continue;
                        }
                        match fs::read_to_string(&entry) {
                            Ok(json) => match MetadataRecord::from_json(&json) {
                                Ok(record) => on_record(&record),
                                Err(source) => on_bad(
                                    &entry,
                                    &DeviceError::Record {
                                        path: entry.clone(),
                                        source,
                                    },
                                ),
                            },
                            Err(e) => on_bad(&entry, &io_error(&entry, e)),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Delete temporary files older than `older_than` anywhere under the
    /// objects tree (10.11). Returns the paths removed.
    pub fn cleanup_temporaries(&self, older_than: Duration) -> Result<Vec<PathBuf>, DeviceError> {
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = Vec::new();
        let bucket = self.root.join(OBJECTS_DIR).join(DEFAULT_BUCKET);
        for first in read_dir_sorted(&bucket)? {
            for second in read_dir_sorted(&first)? {
                for key_dir in read_dir_sorted(&second)? {
                    for entry in read_dir_sorted(&key_dir)? {
                        let name = entry
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        if !name.ends_with(TEMPORARY_SUFFIX) {
                            continue;
                        }
                        let modified = fs::metadata(&entry)
                            .and_then(|m| m.modified())
                            .map_err(|e| io_error(&entry, e))?;
                        if modified < cutoff {
                            remove_if_present(&entry)?;
                            removed.push(entry);
                        }
                    }
                }
            }
        }
        Ok(removed)
    }
}

fn remove_if_present(path: &Path) -> Result<(), DeviceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(path, e)),
    }
}

pub(crate) fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, DeviceError> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(e) => return Err(io_error(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error(dir, e))?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// A shard file being written. Finish it to rename it into place, or drop
/// it to discard the temporary file.
pub struct ShardWrite {
    writer: Option<ShardFileWriter>,
    temp_path: PathBuf,
    final_path: PathBuf,
    expected_length: u64,
}

impl ShardWrite {
    pub fn append_block(&mut self, block: &ShardBlock) -> Result<(), DeviceError> {
        let writer = self
            .writer
            .as_mut()
            .expect("writer present until finish or abort");
        Ok(writer.append_block(block)?)
    }

    /// Write footer and trailer, fsync, rename into place, fsync the
    /// directory (9.3.3). The finished file must be exactly the reserved
    /// length; the geometry check inside `finish` guarantees it.
    pub fn finish(
        mut self,
        object_size: u64,
        object_checksum: BlockChecksum,
    ) -> Result<ShardFileFooter, DeviceError> {
        let writer = self
            .writer
            .take()
            .expect("writer present until finish or abort");
        let footer = match writer.finish(object_size, object_checksum) {
            Ok(footer) => footer,
            Err(e) => {
                let _ = fs::remove_file(&self.temp_path);
                return Err(e.into());
            }
        };
        let actual_length = fs::metadata(&self.temp_path)
            .map(|m| m.len())
            .map_err(|e| io_error(&self.temp_path, e))?;
        debug_assert_eq!(actual_length, self.expected_length);
        if let Err(e) = fs::rename(&self.temp_path, &self.final_path) {
            let _ = fs::remove_file(&self.temp_path);
            return Err(io_error(&self.final_path, e));
        }
        let dir = self.final_path.parent().expect("a file path has a parent");
        fsync_directory(dir)?;
        Ok(footer)
    }

    /// Discard the temporary file.
    pub fn abort(mut self) {
        self.writer = None;
        let _ = fs::remove_file(&self.temp_path);
    }

    pub fn temporary_path(&self) -> &Path {
        &self.temp_path
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

impl Drop for ShardWrite {
    fn drop(&mut self) {
        // A write that was neither finished nor aborted, for example
        // because the connection dropped mid-stream, leaves nothing behind.
        if self.writer.is_some() {
            self.writer = None;
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}
