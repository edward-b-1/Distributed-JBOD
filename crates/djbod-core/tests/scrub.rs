//! Tests of the scrubber against devices in temporary directories.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use djbod_core::checksum::checksum_block;
use djbod_core::device::Device;
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::hash_key;
use djbod_core::layout::{record_file_name, shard_file_name};
use djbod_core::record::{MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME};
use djbod_core::scrub::{scrub_device, Finding, ScrubOptions};
use djbod_core::shardfile::ShardFileHeader;
use djbod_core::stripe::encode_stripe;
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

/// Store `object` under `key` on `devices` (one shard each), with records
/// everywhere. Returns the record.
fn store(
    devices: &[Device],
    scheme: Scheme,
    key: &str,
    version: VersionId,
    object: &[u8],
) -> MetadataRecord {
    let code = ReedSolomonCode::new(scheme);
    let key_hash = hash_key(key.as_bytes());
    let object_checksum = checksum_block(object);
    let mut writes = Vec::new();
    let mut shards = Vec::new();
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
                .expect("begin"),
        );
        shards.push(ShardLocation {
            index: index.0,
            device: devices[i].id(),
        });
    }
    for stripe in object.chunks(scheme.data_shards() * BLOCK) {
        for block in encode_stripe(&code, stripe, BLOCK).expect("encode") {
            writes[block.index.as_usize()]
                .append_block(&block)
                .expect("append");
        }
    }
    for write in writes {
        write
            .finish(object.len() as u64, object_checksum)
            .expect("finish");
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
        revision: 0,
    };
    for device in devices {
        device.write_record(&record).expect("write record");
    }
    record
}

fn scrub(device: &Device) -> Vec<Finding> {
    let mut seen = Vec::new();
    let summary = scrub_device(device, &ScrubOptions::default(), &mut |f| {
        seen.push(f.clone())
    })
    .expect("scrub");
    assert_eq!(seen, summary.findings, "callback and summary must agree");
    summary.findings
}

fn shard_path(device: &Device, key: &str, record: &MetadataRecord) -> std::path::PathBuf {
    let index = record.shard_on(device.id()).expect("device holds a shard");
    device
        .object_directory(&hash_key(key.as_bytes()))
        .join(shard_file_name(&record.version, index))
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).expect("read");
    bytes[offset] ^= 0x01;
    fs::write(path, &bytes).expect("write");
}

#[test]
fn a_clean_device_has_no_findings_and_counts_everything() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), Uuid::from_u128(1)).expect("init"))
        .collect();
    let scheme = Scheme::new(2, 1).expect("scheme");
    for i in 0..5u8 {
        store(
            &devices,
            scheme,
            &format!("key-{i}"),
            VersionId([i + 1; 16]),
            &xorshift64_bytes(3 * 2 * BLOCK + 10, i as u64),
        );
    }
    let summary = scrub_device(&devices[0], &ScrubOptions::default(), &mut |_| {}).expect("scrub");
    assert!(summary.findings.is_empty(), "{:?}", summary.findings);
    assert_eq!(summary.records_checked, 5);
    assert_eq!(summary.shards_checked, 5);
    assert_eq!(summary.blocks_checked, 5 * 4);
    assert!(summary.bytes_read > 5 * 4 * BLOCK as u64);
    assert_eq!(summary.device, Some(devices[0].id()));
}

#[test]
fn corrupt_blocks_are_reported_with_their_stripes_and_the_key() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), Uuid::from_u128(1)).expect("init"))
        .collect();
    let scheme = Scheme::new(1, 1).expect("scheme");
    let record = store(
        &devices,
        scheme,
        "k",
        VersionId([1u8; 16]),
        &xorshift64_bytes(5 * BLOCK, 1),
    );
    let path = shard_path(&devices[0], "k", &record);
    flip_byte(&path, 4096 + BLOCK + 3); // stripe 1
    flip_byte(&path, 4096 + 3 * BLOCK + 3); // stripe 3

    let findings = scrub(&devices[0]);
    assert_eq!(findings.len(), 1);
    match &findings[0] {
        Finding::ShardBlocksCorrupt {
            key,
            version,
            shard_index,
            stripes,
            ..
        } => {
            assert_eq!(key.as_deref(), Some("k"));
            assert_eq!(*version, record.version);
            assert_eq!(*shard_index, 0);
            assert_eq!(stripes, &vec![1, 3]);
        }
        other => panic!("expected ShardBlocksCorrupt, got {other:?}"),
    }
    assert_eq!(findings[0].repair_key(), Some("k"));
    // The other device is untouched.
    assert!(scrub(&devices[1]).is_empty());
}

#[test]
fn structural_damage_and_absence_are_reported() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), Uuid::from_u128(1)).expect("init"))
        .collect();
    let scheme = Scheme::new(1, 1).expect("scheme");
    let record = store(
        &devices,
        scheme,
        "k",
        VersionId([1u8; 16]),
        &xorshift64_bytes(2 * BLOCK, 2),
    );

    // Append a byte: the trailer no longer describes the file.
    let path = shard_path(&devices[0], "k", &record);
    let mut bytes = fs::read(&path).expect("read");
    bytes.push(b'\n');
    fs::write(&path, &bytes).expect("write");
    let findings = scrub(&devices[0]);
    assert_eq!(findings.len(), 1);
    assert!(
        matches!(&findings[0], Finding::ShardUnreadable { key: Some(k), reason, .. } if k == "k" && reason.contains("trailer"))
    );

    // Delete the shard: the record is here but its shard is not.
    fs::remove_file(&path).expect("remove");
    let findings = scrub(&devices[0]);
    assert_eq!(findings.len(), 1);
    assert!(
        matches!(&findings[0], Finding::RecordWithoutShard { key, shard_index: 0, .. } if key == "k")
    );
    assert_eq!(findings[0].repair_key(), Some("k"));

    // Delete the record instead: a shard with no record.
    let dir = devices[1].object_directory(&hash_key(b"k"));
    fs::remove_file(dir.join(record_file_name(&record.version))).expect("remove record");
    let findings = scrub(&devices[1]);
    assert_eq!(findings.len(), 1);
    assert!(matches!(
        &findings[0],
        Finding::ShardWithoutRecord { shard_index: 1, .. }
    ));
    assert_eq!(findings[0].repair_key(), None);
}

#[test]
fn a_damaged_record_is_reported_and_its_shard_still_checked() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), Uuid::from_u128(1)).expect("init"))
        .collect();
    let scheme = Scheme::new(1, 1).expect("scheme");
    let record = store(
        &devices,
        scheme,
        "k",
        VersionId([1u8; 16]),
        &xorshift64_bytes(BLOCK, 3),
    );
    let dir = devices[0].object_directory(&hash_key(b"k"));
    let record_path = dir.join(record_file_name(&record.version));
    let text = fs::read_to_string(&record_path).expect("read");
    fs::write(
        &record_path,
        text.replace("\"size\": 4096", "\"size\": 4097"),
    )
    .expect("write");

    let findings = scrub(&devices[0]);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        matches!(&findings[0], Finding::RecordCorrupt { reason, .. } if reason.contains("checksum"))
    );

    // A record moved into the wrong key directory is caught too.
    let wrong_dir = devices[0].object_directory(&hash_key(b"other"));
    fs::create_dir_all(&wrong_dir).expect("mkdir");
    fs::write(wrong_dir.join(record_file_name(&record.version)), text).expect("write");
    let findings = scrub(&devices[0]);
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings.iter().any(|f| matches!(f, Finding::RecordCorrupt { reason, .. } if reason.contains("different key hash"))));
}

#[test]
fn misplaced_shard_headers_and_stale_temporaries_are_reported() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), Uuid::from_u128(1)).expect("init"))
        .collect();
    let scheme = Scheme::new(1, 1).expect("scheme");
    let record = store(
        &devices,
        scheme,
        "k",
        VersionId([1u8; 16]),
        &xorshift64_bytes(BLOCK, 4),
    );

    // Rename shard 1's file to claim it is shard 0: the header disagrees.
    let dir = devices[1].object_directory(&hash_key(b"k"));
    let from = dir.join(shard_file_name(&record.version, ShardIndex(1)));
    let to = dir.join(shard_file_name(&record.version, ShardIndex(0)));
    fs::rename(&from, &to).expect("rename");
    let findings = scrub(&devices[1]);
    assert!(findings.iter().any(|f| matches!(f, Finding::ShardMisplaced { reason, .. } if reason.contains("file name says shard 0"))), "{findings:?}");
    assert!(
        findings
            .iter()
            .any(|f| matches!(f, Finding::RecordWithoutShard { shard_index: 1, .. })),
        "{findings:?}"
    );

    // An old temporary is reported; a fresh one is not.
    let stale = dir.join("stale.0.shard.tmp");
    fs::write(&stale, b"x").expect("write");
    fs::File::open(&stale)
        .expect("open")
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("mtime");
    fs::write(dir.join("fresh.0.shard.tmp"), b"x").expect("write");
    let findings = scrub(&devices[1]);
    let stale_findings: Vec<&Finding> = findings
        .iter()
        .filter(|f| matches!(f, Finding::StaleTemporary { .. }))
        .collect();
    assert_eq!(stale_findings.len(), 1);
    assert!(
        matches!(stale_findings[0], Finding::StaleTemporary { age_secs, .. } if *age_secs >= 7000)
    );
}

#[test]
fn rate_limit_slows_the_scrub() {
    let dir = tempfile::tempdir().expect("dir");
    let device = Device::initialise(dir.path(), Uuid::from_u128(1)).expect("init");
    let scheme = Scheme::new(1, 0).expect("scheme");
    store(
        std::slice::from_ref(&device),
        scheme,
        "k",
        VersionId([1u8; 16]),
        &xorshift64_bytes(64 * BLOCK, 5),
    );
    let options = ScrubOptions {
        max_bytes_per_second: Some(512 * 1024), // 256 KiB of blocks takes at least half a second
        temporary_max_age: Duration::from_secs(3600),
    };
    let started = std::time::Instant::now();
    let summary = scrub_device(&device, &options, &mut |_| {}).expect("scrub");
    assert!(summary.findings.is_empty());
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "took {:?}",
        started.elapsed()
    );
}
