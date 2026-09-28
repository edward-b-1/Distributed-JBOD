//! Handle-based access kept private to the testing tool.
use crate::model::{check_record, Fingerprint, Mutation, Probe};
use anyhow::{ensure, Context, Result};
use djbod_core::{
    checksum::checksum_block,
    device::{DeviceIdentity, DEVICE_FORMAT_VERSION, DEVICE_IDENTITY_FILE},
    erasure::ShardIndex,
    layout::{object_directory, record_file_name, shard_file_name, DEFAULT_BUCKET},
    record::{DeviceId, MetadataRecord, SYSTEM_NAME},
    shardfile::{
        shard_geometry, ShardFileFooter, ShardFileHeader, FOOTER_FIXED_LEN, HEADER_LEN, TRAILER_LEN,
    },
};
use rustix::fs::{flock, open, openat, FlockOperation, Mode, OFlags};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_FOOTER: u64 = 64 * 1024 * 1024;

pub(crate) fn directory(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute(),
        "path must be absolute: {}",
        path.display()
    );
    let mut fd = File::from(open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                fd = File::from(
                    openat(
                        &fd,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .with_context(|| {
                        format!("opening directory without symlinks: {}", path.display())
                    })?,
                );
            }
            _ => anyhow::bail!("parent traversal is not allowed: {}", path.display()),
        }
    }
    Ok(fd)
}

pub(crate) fn outside_devices(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for parent in absolute.ancestors().skip(1) {
        ensure!(
            !parent.join(DEVICE_IDENTITY_FILE).exists(),
            "plan or journal must be outside device roots"
        );
    }
    Ok(())
}

pub(crate) fn regular(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "target must be a regular file with exactly one hard link"
    );
    Ok(())
}

pub(crate) fn fingerprint(file: &File) -> Result<Fingerprint> {
    let m = file.metadata()?;
    Ok(Fingerprint {
        device: m.dev(),
        inode: m.ino(),
        length: m.len(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
    })
}

fn relative_file(root: &File, relative: &Path, writable: bool) -> Result<File> {
    let components: Vec<_> = relative.components().collect();
    ensure!(!components.is_empty(), "empty relative path");
    let mut parent = root.try_clone()?;
    for (i, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("unsafe relative path");
        };
        let last = i + 1 == components.len();
        let access = if last && writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let mut flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if !last {
            flags |= OFlags::DIRECTORY;
        }
        parent = File::from(openat(&parent, *name, flags, Mode::empty())?);
    }
    regular(&parent)?;
    Ok(parent)
}

fn identity(root: &File) -> Result<DeviceIdentity> {
    let file = relative_file(root, Path::new(DEVICE_IDENTITY_FILE), false)?;
    let mut data = String::new();
    file.take(MAX_METADATA + 1).read_to_string(&mut data)?;
    ensure!(data.len() as u64 <= MAX_METADATA, "identity too large");
    let value: DeviceIdentity = serde_json::from_str(&data)?;
    ensure!(
        value.system == SYSTEM_NAME && value.format_version == DEVICE_FORMAT_VERSION,
        "invalid device identity"
    );
    Ok(value)
}

pub(crate) struct Root {
    pub path: PathBuf,
    pub id: DeviceId,
    cluster: Uuid,
    fd: File,
}

impl Root {
    pub fn open(path: &Path, cluster: Uuid) -> Result<Self> {
        let fd = directory(path)?;
        // Product processes do not take this advisory lock. It excludes a
        // second bitrotter worker using the same device through another path.
        flock(&fd, FlockOperation::NonBlockingLockExclusive)
            .context("another bitrotter worker owns this device")?;
        let identity = identity(&fd)?;
        ensure!(
            identity.cluster_id == cluster,
            "device belongs to a different cluster"
        );
        Ok(Self {
            path: path.to_path_buf(),
            id: identity.device_id,
            cluster,
            fd,
        })
    }
    fn present(&self) -> Result<()> {
        let current = directory(&self.path)?;
        let before = self.fd.metadata()?;
        let after = current.metadata()?;
        ensure!(
            before.dev() == after.dev() && before.ino() == after.ino(),
            "device mount/root changed"
        );
        let id = identity(&self.fd)?;
        ensure!(
            id.device_id == self.id && id.cluster_id == self.cluster,
            "device identity changed"
        );
        Ok(())
    }
    pub fn shard(&self, record: &MetadataRecord, index: u8, writable: bool) -> Result<Shard> {
        check_record(record)?;
        ensure!(
            record.device_for(ShardIndex(index)) == Some(self.id),
            "record does not place this shard on this device"
        );
        self.present()?;
        let dir = object_directory(Path::new(""), DEFAULT_BUCKET, &record.key_hash);
        let mut json = String::new();
        let record_file = relative_file(
            &self.fd,
            &dir.join(record_file_name(&record.version)),
            false,
        )?;
        record_file
            .take(MAX_METADATA + 1)
            .read_to_string(&mut json)?;
        ensure!(json.len() as u64 <= MAX_METADATA, "metadata too large");
        ensure!(
            MetadataRecord::from_json(&json)? == *record,
            "placement or metadata changed"
        );
        let relative = dir.join(shard_file_name(&record.version, ShardIndex(index)));
        let file = relative_file(&self.fd, &relative, writable)?;
        Shard::open(file, self.path.join(relative), record, index)
    }
}

pub(crate) struct Shard {
    pub file: File,
    pub path: PathBuf,
    header: ShardFileHeader,
    footer: ShardFileFooter,
}

impl Shard {
    fn open(file: File, path: PathBuf, record: &MetadataRecord, index: u8) -> Result<Self> {
        let length = file.metadata()?.len();
        ensure!(
            length >= HEADER_LEN + FOOTER_FIXED_LEN + TRAILER_LEN + 9,
            "shard too short"
        );
        let mut trailer = [0; TRAILER_LEN as usize];
        file.read_exact_at(&mut trailer, length - TRAILER_LEN)?;
        let footer_length = u64::from_le_bytes(trailer[..8].try_into()?);
        let footer_offset = u64::from_le_bytes(trailer[8..].try_into()?);
        ensure!(
            (FOOTER_FIXED_LEN..=MAX_FOOTER).contains(&footer_length),
            "invalid or excessive footer length"
        );
        ensure!(
            (footer_length - FOOTER_FIXED_LEN).is_multiple_of(8),
            "invalid checksum table length"
        );
        ensure!(
            footer_offset >= HEADER_LEN
                && footer_offset
                    .checked_add(footer_length)
                    .and_then(|n| n.checked_add(TRAILER_LEN))
                    == Some(length),
            "footer does not describe this file"
        );
        let mut footer_bytes = vec![0; (footer_length + TRAILER_LEN) as usize];
        file.read_exact_at(&mut footer_bytes, footer_offset)?;
        let footer = ShardFileFooter::decode_with_trailer(&footer_bytes)?;
        let mut header_bytes = vec![0; HEADER_LEN as usize];
        file.read_exact_at(&mut header_bytes, 0)?;
        let header = ShardFileHeader::decode(&header_bytes)?;
        ensure!(
            header.key_hash == record.key_hash
                && header.version_id == record.version
                && header.shard_index.0 == index
                && header.scheme == record.scheme()?
                && header.block_length == record.block_size,
            "shard header differs from record"
        );
        let geometry = shard_geometry(header.scheme, header.block_length, record.size)
            .context("invalid geometry")?;
        ensure!(
            footer.block_count == geometry.block_count
                && footer.last_block_length == geometry.last_block_length
                && footer.object_size == record.size
                && footer.object_checksum == record.object_checksum,
            "shard footer differs from record"
        );
        let expected_offset = (footer.block_count - 1)
            .checked_mul(header.block_length)
            .and_then(|n| n.checked_add(HEADER_LEN))
            .and_then(|n| n.checked_add(footer.last_block_length))
            .context("geometry overflow")?;
        ensure!(
            expected_offset == footer_offset,
            "payload boundaries do not match footer"
        );
        Ok(Self {
            file,
            path,
            header,
            footer,
        })
    }
    pub fn block(&self, stripe: u64) -> Result<(u64, Vec<u8>)> {
        ensure!(stripe < self.footer.block_count, "stripe out of range");
        let length = if stripe + 1 == self.footer.block_count {
            self.footer.last_block_length
        } else {
            self.header.block_length
        };
        let offset = stripe
            .checked_mul(self.header.block_length)
            .and_then(|v| v.checked_add(HEADER_LEN))
            .context("block offset overflow")?;
        let mut bytes = vec![0; length as usize];
        self.file.read_exact_at(&mut bytes, offset)?;
        Ok((offset, bytes))
    }
    pub fn probe(&self, stripe: u64) -> Result<Probe> {
        let (_, bytes) = self.block(stripe)?;
        let actual = checksum_block(&bytes).0;
        let stored = self.footer.checksums[stripe as usize].0;
        Ok(Probe {
            index: self.header.shard_index.0,
            intact: actual == stored,
            fingerprint: fingerprint(&self.file)?,
            stored_checksum: stored,
            actual_checksum: actual,
        })
    }
    pub fn prepare(&self, stripe: u64, byte: u64, mask: u8, device: DeviceId) -> Result<Mutation> {
        let (offset, mut bytes) = self.block(stripe)?;
        ensure!(
            byte < bytes.len() as u64 && mask.is_power_of_two(),
            "invalid bit position"
        );
        let before_checksum = checksum_block(&bytes).0;
        ensure!(
            before_checksum == self.footer.checksums[stripe as usize].0,
            "already_corrupt: selected block is not intact"
        );
        let before = bytes[byte as usize];
        let after = before ^ mask;
        bytes[byte as usize] = after;
        let after_checksum = checksum_block(&bytes).0;
        ensure!(
            before_checksum != after_checksum,
            "bit change did not change block checksum"
        );
        Ok(Mutation {
            index: self.header.shard_index.0,
            device,
            path: self.path.clone(),
            fingerprint: fingerprint(&self.file)?,
            offset: offset + byte,
            mask,
            before,
            after,
            before_checksum,
            after_checksum,
        })
    }
    pub fn apply(&self, stripe: u64, mutation: &Mutation, intent_exists: bool) -> Result<()> {
        let current = fingerprint(&self.file)?;
        ensure!(
            current.same_file(&mutation.fingerprint),
            "uncertain: shard file was replaced"
        );
        let (offset, bytes) = self.block(stripe)?;
        ensure!(
            mutation.offset >= offset
                && mutation.offset - offset < bytes.len() as u64
                && mutation.mask.is_power_of_two()
                && mutation.before ^ mutation.mask == mutation.after,
            "invalid journaled payload mutation"
        );
        let checksum = checksum_block(&bytes).0;
        let mut value = [0];
        self.file.read_exact_at(&mut value, mutation.offset)?;
        if intent_exists && checksum == mutation.after_checksum && value[0] == mutation.after {
            self.file.sync_all()?;
            return Ok(()); // Lost acknowledgement or crash after the write.
        }
        ensure!(
            current == mutation.fingerprint
                && checksum == mutation.before_checksum
                && value[0] == mutation.before,
            "uncertain: shard changed since preparation"
        );
        // Write the planned value, never toggle an unexamined current byte.
        self.file.write_all_at(&[mutation.after], mutation.offset)?;
        self.file.sync_all()?;
        let (_, bytes) = self.block(stripe)?;
        ensure!(
            checksum_block(&bytes).0 == mutation.after_checksum,
            "uncertain: post-write checksum differs"
        );
        ensure!(
            fingerprint(&self.file)?.same_file(&mutation.fingerprint),
            "uncertain: file identity changed"
        );
        Ok(())
    }
}
