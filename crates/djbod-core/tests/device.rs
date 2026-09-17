//! Tests of the device layer (SPEC 5, 9.3.3, 9.4.3, 10.6, 10.11) using
//! temporary directories as devices.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use djbod_core::checksum::checksum_block;
use djbod_core::device::{Device, DeviceError, DEVICE_IDENTITY_FILE, TEMPORARY_SUFFIX};
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::{hash_key, KeyHash};
use djbod_core::layout::{record_file_name, shard_file_name};
use djbod_core::record::{MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME};
use djbod_core::shardfile::{shard_file_length, ShardFileHeader};
use djbod_core::stripe::{decode_stripe, encode_stripe, DecodedStripe};
use djbod_core::version::VersionId;
use time::OffsetDateTime;
use uuid::Uuid;

const BLOCK: usize = 4096;

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

fn cluster() -> Uuid {
    Uuid::from_u128(0xC1)
}

fn new_device(dir: &Path) -> Device {
    Device::initialise(dir, cluster()).expect("failed to initialise device")
}

/// Write `object` under `key` across `devices` (one shard each) with the
/// given scheme, returning the record that describes it.
fn store_object(
    devices: &[Device],
    scheme: Scheme,
    key: &str,
    version: VersionId,
    object: &[u8],
) -> MetadataRecord {
    let code = ReedSolomonCode::new(scheme);
    let key_hash = hash_key(key.as_bytes());
    let object_checksum = checksum_block(object);
    let mut writes = Vec::with_capacity(scheme.total_shards());
    let mut shards = Vec::with_capacity(scheme.total_shards());
    for (i, index) in scheme.shard_indices().into_iter().enumerate() {
        let header = ShardFileHeader {
            scheme,
            shard_index: index,
            block_length: BLOCK as u64,
            key_hash,
            version_id: version,
        };
        writes.push(
            devices[i]
                .begin_shard(&key_hash, header, object.len() as u64)
                .expect("failed to begin shard"),
        );
        shards.push(ShardLocation {
            index: index.0,
            device: devices[i].id(),
        });
    }
    for stripe in object.chunks(scheme.data_shards() * BLOCK) {
        let blocks = encode_stripe(&code, stripe, BLOCK).expect("failed to encode stripe");
        for block in &blocks {
            writes[block.index.as_usize()]
                .append_block(block)
                .expect("failed to append block");
        }
    }
    for write in writes {
        write
            .finish(object.len() as u64, object_checksum)
            .expect("failed to finish shard");
    }
    let record = MetadataRecord {
        format_version: RECORD_FORMAT_VERSION,
        system: SYSTEM_NAME.to_string(),
        bucket: "default".to_string(),
        key: key.to_string(),
        key_hash,
        version,
        created: OffsetDateTime::now_utc(),
        size: object.len() as u64,
        object_checksum,
        k: scheme.data_shards() as u8,
        m: scheme.parity_shards() as u8,
        block_size: BLOCK as u64,
        shards,
        content_type: None,
        user_metadata: BTreeMap::new(),
    };
    for device in devices {
        device
            .write_record(&record)
            .expect("failed to write record");
    }
    record
}

/// Read `record`'s object back from `devices` through the data shards.
fn load_object(devices: &[Device], record: &MetadataRecord) -> Vec<u8> {
    let scheme = record.scheme().expect("valid scheme");
    let code = ReedSolomonCode::new(scheme);
    let data_indices = scheme.data_shard_indices();
    let mut readers = Vec::new();
    for index in &data_indices {
        let device_id = record.device_for(*index).expect("record lists every index");
        let device = devices
            .iter()
            .find(|d| d.id() == device_id)
            .expect("device present");
        readers.push(
            device
                .open_shard(&record.key_hash, &record.version, *index)
                .expect("failed to open shard"),
        );
    }
    let stripe_size = scheme.data_shards() * BLOCK;
    let mut out = Vec::with_capacity(record.size as usize);
    for stripe_number in 0..readers[0].block_count() {
        let mut received = Vec::new();
        for reader in &readers {
            received.push(
                reader
                    .read_block(stripe_number)
                    .expect("failed to read block"),
            );
        }
        let start = stripe_number as usize * stripe_size;
        let len = (record.size as usize - start).min(stripe_size);
        match decode_stripe(&code, &data_indices, &received, len).expect("failed to decode") {
            DecodedStripe::Intact { data } => out.extend_from_slice(&data),
            other => panic!("expected Intact, got {other:?}"),
        }
    }
    out
}

#[test]
fn initialise_writes_a_searchable_identity_file_and_open_reads_it_back() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());

    let identity_path = dir.path().join(DEVICE_IDENTITY_FILE);
    assert_eq!(DEVICE_IDENTITY_FILE, "DISTRIBUTED-JBOD-DEVICE.json");
    let text = fs::read_to_string(&identity_path).expect("failed to read identity file");
    assert!(text.contains("\"system\": \"distributed-jbod\""));
    assert!(text.contains("Distributed-JBOD"));
    assert!(text.contains("Do not add, edit, move, or delete"));
    assert!(text.contains(&device.id().0.to_string()));
    assert!(dir.path().join("objects").join("default").is_dir());

    let reopened = Device::open(dir.path(), Some(cluster())).expect("failed to open device");
    assert_eq!(reopened.id(), device.id());
    assert_eq!(reopened.identity(), device.identity());
    assert_eq!(reopened.filesystem_id(), device.filesystem_id());
}

#[test]
fn initialise_requires_an_empty_directory_but_tolerates_lost_and_found() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(dir.path().join("lost+found")).expect("failed to create lost+found");
    new_device(dir.path());

    let dir = tempfile::tempdir().expect("failed to create temp dir");
    fs::write(dir.path().join("holiday-photos.zip"), b"x").expect("failed to write file");
    fs::create_dir(dir.path().join("lost+found")).expect("failed to create lost+found");
    match Device::initialise(dir.path(), cluster()) {
        Err(DeviceError::NotEmpty { found, .. }) => {
            assert_eq!(found, vec!["holiday-photos.zip".to_string()]);
        }
        other => panic!("expected NotEmpty, got {other:?}"),
    }

    // Initialising twice is refused: the identity file is a file.
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    new_device(dir.path());
    assert!(matches!(
        Device::initialise(dir.path(), cluster()),
        Err(DeviceError::NotEmpty { .. })
    ));

    let file = tempfile::NamedTempFile::new().expect("failed to create temp file");
    assert!(matches!(
        Device::initialise(file.path(), cluster()),
        Err(DeviceError::NotADirectory { .. })
    ));
}

#[test]
fn open_distinguishes_uninitialised_foreign_and_wrong_cluster_directories() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    assert!(matches!(
        Device::open(dir.path(), None),
        Err(DeviceError::NotInitialised { .. })
    ));

    new_device(dir.path());
    fs::remove_file(dir.path().join(DEVICE_IDENTITY_FILE)).expect("failed to delete identity");
    assert!(matches!(
        Device::open(dir.path(), None),
        Err(DeviceError::ForeignDirectory { .. })
    ));

    let dir = tempfile::tempdir().expect("failed to create temp dir");
    new_device(dir.path());
    match Device::open(dir.path(), Some(Uuid::from_u128(0xC2))) {
        Err(DeviceError::WrongCluster {
            expected, actual, ..
        }) => {
            assert_eq!(expected, Uuid::from_u128(0xC2));
            assert_eq!(actual, cluster());
        }
        other => panic!("expected WrongCluster, got {other:?}"),
    }
    Device::open(dir.path(), None).expect("open without an expected cluster should succeed");

    let dir = tempfile::tempdir().expect("failed to create temp dir");
    new_device(dir.path());
    fs::write(dir.path().join(DEVICE_IDENTITY_FILE), "{ not json").expect("failed to corrupt");
    assert!(matches!(
        Device::open(dir.path(), None),
        Err(DeviceError::BadIdentity { .. })
    ));
}

#[test]
fn free_space_is_positive_and_headroom_reduces_it() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let full = device.free_space(0.0).expect("failed to read free space");
    let with_headroom = device.free_space(0.05).expect("failed to read free space");
    assert!(full > 0);
    assert!(with_headroom < full);
}

#[test]
fn a_stored_object_reads_back_and_lives_where_the_layout_says() {
    let dirs: Vec<_> = (0..4)
        .map(|_| tempfile::tempdir().expect("failed to create temp dir"))
        .collect();
    let devices: Vec<Device> = dirs.iter().map(|d| new_device(d.path())).collect();
    let scheme = Scheme::new(3, 1).expect("valid scheme");
    let object = xorshift64_bytes(5 * 3 * BLOCK + 777, 1);
    let version = VersionId::from_text("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID");

    let record = store_object(&devices, scheme, "docs/report.pdf", version, &object);
    assert_eq!(load_object(&devices, &record), object);

    // Files are exactly where SPEC 9.1 says, with no temporaries left.
    let key_hash = hash_key(b"docs/report.pdf");
    for (i, device) in devices.iter().enumerate() {
        let dir = device.object_directory(&key_hash);
        assert!(dir.starts_with(dirs[i].path().join("objects").join("default")));
        let shard_path = dir.join(shard_file_name(&version, ShardIndex(i as u8)));
        assert!(shard_path.is_file(), "{shard_path:?}");
        assert_eq!(
            fs::metadata(&shard_path).expect("stat").len(),
            shard_file_length(scheme, BLOCK as u64, object.len() as u64).expect("geometry")
        );
        assert!(dir.join(record_file_name(&version)).is_file());
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "unexpected files: {names:?}");
        assert!(names.iter().all(|n| !n.ends_with(TEMPORARY_SUFFIX)));
    }

    // Records read back equal, oldest first, and by version.
    let records = devices[0]
        .read_records(&key_hash)
        .expect("failed to read records");
    assert_eq!(records, vec![record.clone()]);
    assert_eq!(
        devices[1]
            .read_record(&key_hash, &version)
            .expect("read record"),
        record
    );
    assert!(devices[0]
        .read_records(&hash_key(b"nothing here"))
        .expect("read")
        .is_empty());
}

#[test]
fn a_dropped_or_aborted_shard_write_leaves_nothing_behind() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let scheme = Scheme::new(2, 1).expect("valid scheme");
    let key_hash = hash_key(b"k");
    let header = ShardFileHeader {
        scheme,
        shard_index: ShardIndex(0),
        block_length: BLOCK as u64,
        key_hash,
        version_id: VersionId([1u8; 16]),
    };

    let write = device
        .begin_shard(&key_hash, header.clone(), 3 * BLOCK as u64)
        .expect("failed to begin shard");
    let temp = write.temporary_path().to_path_buf();
    assert!(temp.is_file());
    assert!(temp.to_string_lossy().ends_with(TEMPORARY_SUFFIX));
    // Reserved to the full final length before any block is written.
    assert_eq!(
        fs::metadata(&temp).expect("stat").len(),
        shard_file_length(scheme, BLOCK as u64, 3 * BLOCK as u64).expect("geometry")
    );
    drop(write);
    assert!(!temp.exists(), "drop should remove the temporary file");

    let write = device
        .begin_shard(&key_hash, header, 3 * BLOCK as u64)
        .expect("failed to begin shard");
    let temp = write.temporary_path().to_path_buf();
    write.abort();
    assert!(!temp.exists());
}

#[test]
fn finishing_with_the_wrong_geometry_removes_the_temporary_file() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let scheme = Scheme::new(1, 0).expect("valid scheme");
    let key_hash = hash_key(b"k");
    let header = ShardFileHeader {
        scheme,
        shard_index: ShardIndex(0),
        block_length: BLOCK as u64,
        key_hash,
        version_id: VersionId([2u8; 16]),
    };
    let mut write = device
        .begin_shard(&key_hash, header, 2 * BLOCK as u64)
        .expect("failed to begin shard");
    let temp = write.temporary_path().to_path_buf();
    let final_path = write.final_path().to_path_buf();
    let block = encode_stripe(&ReedSolomonCode::new(scheme), &[7u8; BLOCK], BLOCK)
        .expect("encode")
        .remove(0);
    write.append_block(&block).expect("append");
    // Only one of the two blocks the object size implies.
    assert!(matches!(
        write.finish(2 * BLOCK as u64, checksum_block(&[])),
        Err(DeviceError::ShardFile(_))
    ));
    assert!(!temp.exists());
    assert!(!final_path.exists());
}

#[test]
fn records_are_validated_and_never_overwritten() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let key = "a";
    let version = VersionId([3u8; 16]);
    let mut record = MetadataRecord {
        format_version: RECORD_FORMAT_VERSION,
        system: SYSTEM_NAME.to_string(),
        bucket: "default".to_string(),
        key: key.to_string(),
        key_hash: hash_key(key.as_bytes()),
        version,
        created: OffsetDateTime::now_utc(),
        size: 1,
        object_checksum: checksum_block(b"x"),
        k: 1,
        m: 0,
        block_size: BLOCK as u64,
        shards: vec![ShardLocation {
            index: 0,
            device: device.id(),
        }],
        content_type: None,
        user_metadata: BTreeMap::new(),
    };
    device
        .write_record(&record)
        .expect("failed to write record");
    assert!(matches!(
        device.write_record(&record),
        Err(DeviceError::RecordExists { .. })
    ));

    record.version = VersionId([4u8; 16]);
    record.key_hash = KeyHash([0u8; 32]);
    assert!(matches!(
        device.write_record(&record),
        Err(DeviceError::Record { .. })
    ));

    // A record on disk whose key does not match the directory it is in.
    let misfiled_dir = device.object_directory(&hash_key(b"b"));
    fs::create_dir_all(&misfiled_dir).expect("mkdir");
    let good = device
        .read_record(&hash_key(b"a"), &version)
        .expect("read record");
    fs::write(
        misfiled_dir.join(record_file_name(&version)),
        good.to_json(),
    )
    .expect("write");
    assert!(matches!(
        device.read_record(&hash_key(b"b"), &version),
        Err(DeviceError::Record { .. })
    ));
}

#[test]
fn delete_removes_record_then_shard_then_empty_directory_and_is_idempotent() {
    let dirs: Vec<_> = (0..2)
        .map(|_| tempfile::tempdir().expect("failed to create temp dir"))
        .collect();
    let devices: Vec<Device> = dirs.iter().map(|d| new_device(d.path())).collect();
    let scheme = Scheme::new(1, 1).expect("valid scheme");
    let object = xorshift64_bytes(3 * BLOCK, 2);
    let version = VersionId([5u8; 16]);
    let record = store_object(&devices, scheme, "to-delete", version, &object);
    let key_hash = record.key_hash;

    let dir = devices[0].object_directory(&key_hash);
    assert!(dir.is_dir());
    devices[0]
        .delete_version(&key_hash, &version)
        .expect("failed to delete");
    assert!(!dir.exists(), "empty key directory should be removed");
    devices[0]
        .delete_version(&key_hash, &version)
        .expect("deleting again should succeed");
    assert!(devices[0].read_records(&key_hash).expect("read").is_empty());

    // The other device still has its copy.
    assert_eq!(devices[1].read_records(&key_hash).expect("read").len(), 1);

    // A directory with another version in it is kept.
    let other = VersionId([6u8; 16]);
    store_object(&devices, scheme, "to-delete", other, &object);
    devices[1]
        .delete_version(&key_hash, &version)
        .expect("delete");
    assert!(devices[1].object_directory(&key_hash).is_dir());
    assert_eq!(devices[1].read_records(&key_hash).expect("read").len(), 1);
}

#[test]
fn walk_records_visits_every_record_and_reports_bad_ones() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let scheme = Scheme::new(1, 0).expect("valid scheme");
    let mut expected_keys = Vec::new();
    for i in 0..20u8 {
        let key = format!("key-{i}");
        let object = xorshift64_bytes(100 + i as usize, i as u64);
        store_object(
            std::slice::from_ref(&device),
            scheme,
            &key,
            VersionId([i; 16]),
            &object,
        );
        expected_keys.push(key);
    }
    // One corrupt record file in a fresh directory.
    let bad_dir = device.object_directory(&hash_key(b"corrupt"));
    fs::create_dir_all(&bad_dir).expect("mkdir");
    fs::write(
        bad_dir.join(record_file_name(&VersionId([99u8; 16]))),
        "nope",
    )
    .expect("write");

    let mut seen = Vec::new();
    let mut bad = Vec::new();
    device
        .walk_records(
            |r| seen.push(r.key.clone()),
            |p, _| bad.push(p.to_path_buf()),
        )
        .expect("walk");
    seen.sort();
    expected_keys.sort();
    assert_eq!(seen, expected_keys);
    assert_eq!(bad.len(), 1);
    assert!(bad[0].starts_with(&bad_dir));
}

#[test]
fn cleanup_removes_only_old_temporaries() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let device = new_device(dir.path());
    let key_dir = device.object_directory(&hash_key(b"k"));
    fs::create_dir_all(&key_dir).expect("mkdir");
    let old = key_dir.join(format!("old.0.shard{TEMPORARY_SUFFIX}"));
    let fresh = key_dir.join(format!("fresh.0.shard{TEMPORARY_SUFFIX}"));
    let real = key_dir.join("real.0.shard");
    for p in [&old, &fresh, &real] {
        fs::write(p, b"x").expect("write");
    }
    let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
    fs::File::open(&old)
        .expect("open")
        .set_modified(two_hours_ago)
        .expect("set mtime");

    let removed = device
        .cleanup_temporaries(Duration::from_secs(3600))
        .expect("cleanup");
    assert_eq!(removed, vec![old.clone()]);
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(real.exists());
}
