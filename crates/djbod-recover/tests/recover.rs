//! `djbod-recover` against device directories written with `djbod-core`.

use std::collections::BTreeMap;
use std::process::Command;

use djbod_core::checksum::checksum_block;
use djbod_core::device::Device;
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::hash_key;
use djbod_core::layout::{record_file_name, shard_file_name};
use djbod_core::record::{MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME};
use djbod_core::shardfile::ShardFileHeader;
use djbod_core::stripe::encode_stripe;
use djbod_core::version::VersionId;
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

const BLOCK: usize = 64 * 1024;

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

/// Write `object` as `key`/`version` across `devices`, one shard each,
/// exactly as a node would (SPEC 9.3, 10).
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
                .expect("begin shard"),
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

fn recover(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_djbod-recover"))
        .args(args)
        .output()
        .expect("run djbod-recover");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn shard_path(device: &Device, record: &MetadataRecord, index: u8) -> std::path::PathBuf {
    device
        .object_directory(&record.key_hash)
        .join(shard_file_name(&record.version, ShardIndex(index)))
}

#[test]
fn listing_aligns_long_unicode_keys_large_numbers_and_orphan_shards() {
    let scheme = Scheme::new(1, 1).expect("scheme");
    let cluster = Uuid::new_v4();
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<_> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), cluster).expect("init"))
        .collect();
    let keys = [
        "short".to_string(),
        "a".repeat(120),
        "界".repeat(21),
        "e\u{301}".repeat(25),
    ];
    for (i, key) in keys.iter().enumerate() {
        let mut record = store_object(&devices, scheme, key, VersionId([i as u8; 16]), b"value");
        if i == 1 {
            // A metadata-only entry lets the listing exercise the full
            // numeric range without allocating an enormous object.
            record.revision = u64::MAX;
            record.size = u64::MAX;
            for (index, device) in devices.iter().enumerate() {
                std::fs::remove_file(shard_path(device, &record, index as u8))
                    .expect("remove shard");
                let path = device
                    .object_directory(&record.key_hash)
                    .join(record_file_name(&record.version));
                std::fs::write(path, record.to_json()).expect("write record");
            }
        }
    }
    let orphan = store_object(&devices, scheme, "unknown", VersionId([9; 16]), b"orphan");
    for device in &devices {
        std::fs::remove_file(
            device
                .object_directory(&orphan.key_hash)
                .join(record_file_name(&orphan.version)),
        )
        .expect("remove record");
    }

    let (ok, out, err) = recover(&[
        "list",
        dirs[0].path().to_str().unwrap(),
        dirs[1].path().to_str().unwrap(),
    ]);
    assert!(
        !ok,
        "missing shards and records must still produce exit status 2"
    );
    assert!(
        err.contains("4 version(s), 2 not recoverable, 0 problem(s)"),
        "{err}"
    );
    assert!(out.contains("no record found; key unknown"), "{out}");
    assert!(out.contains(&u64::MAX.to_string()), "{out}");
    for key in &keys {
        assert!(out.contains(key), "key was truncated: {out}");
    }

    // Check left edges for text and right edges for REV/SIZE, measured in
    // display columns. Single spaces within key hashes/statuses are data.
    let edges = |line: &str| {
        let mut offset = 0;
        let mut columns = Vec::new();
        for cell in line.split("  ") {
            let start = offset + cell.len() - cell.trim_start().len();
            offset += cell.len() + 2;
            if !cell.trim().is_empty() {
                let right = matches!(columns.len(), 2 | 3);
                columns.push(line[..start].width() + if right { cell.trim().width() } else { 0 });
            }
        }
        columns
    };
    let mut lines = out.lines();
    let header = edges(lines.next().expect("header"));
    assert_eq!(header.len(), 6, "{out}");
    let rows: Vec<_> = lines.collect();
    assert_eq!(rows.len(), 5, "{out}");
    for row in rows {
        assert_eq!(edges(row), header, "misaligned row:\n{out}");
    }
}

#[test]
fn lists_and_extracts_from_device_directories_alone() {
    let scheme = Scheme::new(3, 1).expect("scheme");
    let cluster = Uuid::new_v4();
    let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().expect("dir")).collect();
    let devices: Vec<Device> = dirs
        .iter()
        .map(|d| Device::initialise(d.path(), cluster).expect("init"))
        .collect();
    let paths: Vec<&str> = dirs.iter().map(|d| d.path().to_str().unwrap()).collect();
    let older = xorshift64_bytes(2 * 3 * BLOCK + 11, 1);
    let newer = xorshift64_bytes(3 * BLOCK + 5000, 2);
    let v1 = VersionId([1u8; 16]);
    let v2 = VersionId([2u8; 16]);
    let r1 = store_object(&devices, scheme, "photos/cat.jpg", v1, &older);
    let r2 = store_object(&devices, scheme, "photos/cat.jpg", v2, &newer);
    store_object(
        &devices,
        scheme,
        "notes.txt",
        VersionId([3u8; 16]),
        b"hello, recovery",
    );
    // A device without its identity file is still just a directory tree.
    std::fs::remove_file(
        dirs[3]
            .path()
            .join(djbod_core::device::DEVICE_IDENTITY_FILE),
    )
    .expect("remove identity");

    let (ok, out, err) = recover(&[&["list"][..], &paths[..]].concat());
    assert!(ok, "{err}");
    assert!(out.contains("photos/cat.jpg"), "{out}");
    assert!(out.contains("notes.txt"), "{out}");
    assert_eq!(out.matches("4/4").count(), 3, "{out}");
    assert_eq!(out.matches("recoverable").count(), 3, "{out}");
    assert!(
        err.contains("3 version(s), 0 not recoverable, 0 problem(s)"),
        "{err}"
    );

    // Extract: newest by default, an older version by id.
    let out_dir = tempfile::tempdir().expect("dir");
    let newest = out_dir.path().join("cat.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--out",
                newest.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(ok, "{err}");
    assert_eq!(std::fs::read(&newest).expect("read"), newer);
    assert!(err.contains("using shards [0, 1, 2, 3]"), "{err}");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--out",
                newest.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(!ok);
    assert!(err.contains("refusing to overwrite"), "{err}");
    let old_out = out_dir.path().join("cat-old.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--version",
                &v1.to_text(),
                "--out",
                old_out.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(ok, "{err}");
    assert_eq!(std::fs::read(&old_out).expect("read"), older);

    // Damage 1: a corrupt block in shard 1. The file is structurally sound
    // so the listing still counts it; extraction verifies every block and
    // reconstructs around it (m = 1).
    let corrupt = shard_path(&devices[1], &r2, 1);
    let mut bytes = std::fs::read(&corrupt).expect("read");
    bytes[4096 + 10] ^= 0xff;
    std::fs::write(&corrupt, &bytes).expect("write");
    let (ok, out, err) = recover(&[&["list"][..], &paths[..]].concat());
    assert!(ok, "{err}");
    assert_eq!(out.matches("4/4").count(), 3, "{out}");
    let repaired = out_dir.path().join("cat-repaired.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--out",
                repaired.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(ok, "{err}");
    assert_eq!(std::fs::read(&repaired).expect("read"), newer);
    assert!(
        err.contains("stripe 0: reconstructed around shard(s) [1]"),
        "{err}"
    );

    // Damage 2: shard 2 deleted as well. Three files remain, so the
    // listing calls the version recoverable, but stripe 0 now has two
    // erasures and extraction fails before writing anything.
    std::fs::remove_file(shard_path(&devices[2], &r2, 2)).expect("remove");
    let (ok, out, err) = recover(&[&["list"][..], &paths[..]].concat());
    assert!(ok, "{err}");
    assert!(out.contains("3/4"), "{out}");
    let failed = out_dir.path().join("cat-failed.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--out",
                failed.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(!ok);
    assert!(
        err.contains("stripe 0: only 2 usable block(s) of 3 needed; damaged shard(s) [1, 2]"),
        "{err}"
    );
    assert!(!failed.exists());
    assert!(!out_dir.path().join("cat-failed.jpg.partial").exists());

    // Damage 3: shard 3 truncated: structurally damaged, reported, and
    // now only two sound files remain; extraction refuses up front.
    let truncated = shard_path(&devices[3], &r2, 3);
    let bytes = std::fs::read(&truncated).expect("read");
    std::fs::write(&truncated, &bytes[..bytes.len() - 7]).expect("write");
    let (ok, out, err) = recover(&[&["list"][..], &paths[..]].concat());
    assert!(!ok, "damage makes list exit 2");
    assert!(out.contains("2/4"), "{out}");
    assert!(out.contains("NOT recoverable"), "{out}");
    assert!(err.contains("damaged shard file"), "{err}");
    let refused = out_dir.path().join("cat-refused.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--out",
                refused.to_str().unwrap(),
            ][..],
            &paths[..],
        ]
        .concat(),
    );
    assert!(!ok);
    assert!(
        err.contains("only 2 intact shard(s) of 3+1 found; need at least 3"),
        "{err}"
    );
    assert!(!refused.exists());
    assert!(!out_dir.path().join("cat-refused.jpg.partial").exists());

    // The older version is untouched and still extracts from any three
    // of the four directories.
    let three = out_dir.path().join("cat-old-3.jpg");
    let (ok, _, err) = recover(
        &[
            &[
                "extract",
                "photos/cat.jpg",
                "--version",
                &r1.version.to_text(),
                "--out",
                three.to_str().unwrap(),
            ][..],
            &paths[..3],
        ]
        .concat(),
    );
    assert!(ok, "{err}");
    assert_eq!(std::fs::read(&three).expect("read"), older);

    // An unknown key and a directory that is not a device are reported.
    let (ok, _, err) = recover(&[
        "extract",
        "nope",
        "--out",
        out_dir.path().join("nope").to_str().unwrap(),
        paths[0],
    ]);
    assert!(!ok);
    assert!(err.contains("no record of key \"nope\""), "{err}");
    let (ok, _, err) = recover(&["list", out_dir.path().to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("not a device or empty"), "{err}");
}
