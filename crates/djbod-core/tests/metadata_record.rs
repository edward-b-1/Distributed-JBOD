//! Tests of the metadata record (SPEC 9.4): JSON round trip, the shape of
//! the JSON a human sees on disk, and validation of records that are
//! damaged or inconsistent.

use std::collections::BTreeMap;

use djbod_core::checksum::BlockChecksum;
use djbod_core::erasure::ShardIndex;
use djbod_core::keyhash::{hash_key, KeyHash};
use djbod_core::record::{
    DeviceId, MetadataRecord, RecordError, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME,
};
use djbod_core::version::VersionId;
use time::macros::datetime;
use uuid::Uuid;

fn device(n: u128) -> DeviceId {
    DeviceId(Uuid::from_u128(
        0x1000_0000_0000_0000_0000_0000_0000_0000 + n,
    ))
}

fn sample_record() -> MetadataRecord {
    let key = "photos/2026/cat.jpg";
    MetadataRecord {
        format_version: RECORD_FORMAT_VERSION,
        system: SYSTEM_NAME.to_string(),
        bucket: "default".to_string(),
        key: key.to_string(),
        key_hash: hash_key(key.as_bytes()),
        version: VersionId::from_text("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID"),
        created: datetime!(2026-09-17 10:15:30 UTC),
        size: 10 * 1024 * 1024,
        object_checksum: BlockChecksum(0x0b1e_c7c4_3c5a_0001),
        k: 3,
        m: 1,
        block_size: 1 << 20,
        shards: vec![
            ShardLocation {
                index: 0,
                device: device(1),
            },
            ShardLocation {
                index: 1,
                device: device(2),
            },
            ShardLocation {
                index: 2,
                device: device(3),
            },
            ShardLocation {
                index: 3,
                device: device(4),
            },
        ],
        content_type: Some("image/jpeg".to_string()),
        user_metadata: BTreeMap::new(),
        revision: 0,
    }
}

#[test]
fn json_round_trips_exactly() {
    let record = sample_record();
    record.validate().expect("sample record should be valid");
    let json = record.to_json();
    let parsed = MetadataRecord::from_json(&json).expect("failed to parse record");
    assert_eq!(parsed, record);
}

#[test]
fn revision_is_omitted_when_zero_and_covered_by_the_checksum() {
    let record = sample_record();
    assert!(!record.to_json().contains("\"revision\""));

    let mut moved = record.clone();
    moved.revision = 1;
    moved.shards[1].device = device(9);
    let json = moved.to_json();
    assert!(json.contains("\"revision\": 1"), "{json}");
    let parsed = MetadataRecord::from_json(&json).expect("failed to parse record");
    assert_eq!(parsed, moved);
    assert_ne!(moved.checksum(), record.checksum());

    // The two describe the same body and differ only in placement.
    assert!(moved.same_body(&record));
    let mut other = record.clone();
    other.size += 1;
    assert!(!other.same_body(&record));
}

#[test]
fn content_type_is_bounded_and_user_metadata_is_measured() {
    use djbod_core::record::{RecordError, MAX_CONTENT_TYPE_BYTES};
    let mut record = sample_record();
    record.content_type = Some("x".repeat(MAX_CONTENT_TYPE_BYTES));
    record.validate().expect("at the limit is fine");
    record.content_type = Some("x".repeat(MAX_CONTENT_TYPE_BYTES + 1));
    assert!(matches!(
        record.validate(),
        Err(RecordError::ContentTypeTooLong(_))
    ));

    let mut record = sample_record();
    record.user_metadata.insert("k".to_string(), "v".repeat(99));
    record.user_metadata.insert("k2".to_string(), String::new());
    assert_eq!(record.user_metadata_bytes(), 1 + 99 + 2);
}

#[test]
fn json_is_readable_and_uses_the_specified_text_forms() {
    let json = sample_record().to_json();
    // Flat: fields in declaration order, indented for humans, and the
    // checksum as the last field.
    assert!(json.starts_with("{\n  \"format_version\": 1,\n  \"system\": \"distributed-jbod\","));
    assert!(json.ends_with(&format!(
        ",\n  \"checksum\": \"{}\"\n}}",
        sample_record().checksum().to_hex()
    )));
    // And it is still valid JSON that round-trips.
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let record_fields = serde_json::to_value(sample_record()).expect("serialize");
    assert_eq!(
        value.as_object().expect("object").len(),
        record_fields.as_object().expect("object").len() + 1
    );
    // Field names and text forms a human would grep for on a disk.
    assert!(json.contains("\"system\": \"distributed-jbod\""));
    assert!(json.contains("\"key\": \"photos/2026/cat.jpg\""));
    assert!(json.contains(&format!(
        "\"key_hash\": \"{}\"",
        hash_key(b"photos/2026/cat.jpg").to_hex()
    )));
    assert!(json.contains("\"version\": \"01ARZ3NDEKTSV4RRFFQ69G5FAV\""));
    assert!(json.contains("\"created\": \"2026-09-17T10:15:30Z\""));
    assert!(json.contains("\"object_checksum\": \"0b1ec7c43c5a0001\""));
    assert!(json.contains("\"device\": \"10000000-0000-0000-0000-000000000001\""));
    // Optional fields absent when empty.
    assert!(!json.contains("user_metadata"));
    let mut without_type = sample_record();
    without_type.content_type = None;
    assert!(!without_type.to_json().contains("content_type"));
}

#[test]
fn user_metadata_round_trips_when_present() {
    let mut record = sample_record();
    record
        .user_metadata
        .insert("camera".to_string(), "X100".to_string());
    record
        .user_metadata
        .insert("album".to_string(), "cats".to_string());
    let json = record.to_json();
    assert!(json.contains("\"album\": \"cats\""));
    assert_eq!(
        MetadataRecord::from_json(&json).expect("failed to parse"),
        record
    );
}

#[test]
fn lookups_by_index_and_device() {
    let record = sample_record();
    assert_eq!(record.device_for(ShardIndex(2)), Some(device(3)));
    assert_eq!(record.device_for(ShardIndex(4)), None);
    assert_eq!(record.shard_on(device(4)), Some(ShardIndex(3)));
    assert_eq!(record.shard_on(device(9)), None);
    assert_eq!(record.scheme().expect("valid scheme").total_shards(), 4);
}

#[test]
fn a_record_whose_key_does_not_match_its_hash_is_rejected() {
    // Corruption of the key field on disk, or a record misfiled under the
    // wrong directory (SPEC 9.1.6), both look like this.
    let mut record = sample_record();
    record.key = "photos/2026/dog.jpg".to_string();
    assert!(matches!(
        record.validate(),
        Err(RecordError::KeyHashMismatch { .. })
    ));

    let mut record = sample_record();
    record.key_hash = KeyHash([0u8; 32]);
    assert!(matches!(
        record.validate(),
        Err(RecordError::KeyHashMismatch { .. })
    ));
}

#[test]
fn shard_list_must_cover_every_index_once_on_distinct_devices() {
    let mut too_few = sample_record();
    too_few.shards.pop();
    assert_eq!(
        too_few.validate(),
        Err(RecordError::WrongShardCount {
            expected: 4,
            actual: 3
        })
    );

    let mut duplicate_index = sample_record();
    duplicate_index.shards[3].index = 0;
    assert_eq!(
        duplicate_index.validate(),
        Err(RecordError::DuplicateShardIndex(0))
    );

    let mut out_of_range = sample_record();
    out_of_range.shards[3].index = 9;
    assert_eq!(
        out_of_range.validate(),
        Err(RecordError::MissingShardIndex(9))
    );

    let mut same_device = sample_record();
    same_device.shards[3].device = device(1);
    assert_eq!(
        same_device.validate(),
        Err(RecordError::DuplicateDevice(device(1)))
    );
}

#[test]
fn scheme_and_block_size_are_validated() {
    let mut no_data = sample_record();
    no_data.k = 0;
    assert!(matches!(
        no_data.validate(),
        Err(RecordError::InvalidScheme(_))
    ));

    let mut odd_block = sample_record();
    odd_block.block_size = 4096 + 1;
    assert_eq!(odd_block.validate(), Err(RecordError::BadBlockSize(4097)));

    let mut zero_block = sample_record();
    zero_block.block_size = 0;
    assert_eq!(zero_block.validate(), Err(RecordError::BadBlockSize(0)));
}

#[test]
fn system_and_format_version_are_checked() {
    let mut other_system = sample_record();
    other_system.system = "something-else".to_string();
    assert_eq!(
        other_system.validate(),
        Err(RecordError::WrongSystem("something-else".to_string()))
    );

    let mut future = sample_record();
    future.format_version = 2;
    assert_eq!(
        future.validate(),
        Err(RecordError::UnsupportedFormatVersion(2))
    );
}

#[test]
fn malformed_json_and_bad_text_forms_are_rejected() {
    assert!(matches!(
        MetadataRecord::from_json("not json"),
        Err(RecordError::Json(_))
    ));
    assert!(matches!(
        MetadataRecord::from_json("{}"),
        Err(RecordError::Json(_))
    ));

    let json = sample_record().to_json();
    let bad_version = json.replace("01ARZ3NDEKTSV4RRFFQ69G5FAV", "not-a-ulid");
    assert!(matches!(
        MetadataRecord::from_json(&bad_version),
        Err(RecordError::Json(_))
    ));
    let bad_checksum = json.replace("0b1ec7c43c5a0001", "xyz");
    assert!(matches!(
        MetadataRecord::from_json(&bad_checksum),
        Err(RecordError::Json(_))
    ));
    let bad_date = json.replace("2026-09-17T10:15:30Z", "yesterday");
    assert!(matches!(
        MetadataRecord::from_json(&bad_date),
        Err(RecordError::Json(_))
    ));
}

#[test]
fn canonical_form_has_sorted_keys_and_no_whitespace() {
    let canonical = String::from_utf8(sample_record().canonical_bytes()).expect("utf-8");
    assert!(!canonical.contains(' '));
    assert!(!canonical.contains('\n'));
    assert!(canonical.starts_with("{\"block_size\":1048576,\"bucket\":\"default\","));
    // Nested objects are canonical too: shard entries sort "device" before "index".
    assert!(canonical.contains("{\"device\":\"10000000-0000-0000-0000-000000000001\",\"index\":0}"));
    // Absent optional fields do not appear.
    assert!(!canonical.contains("user_metadata"));
    // Stable across calls.
    assert_eq!(
        sample_record().canonical_bytes(),
        sample_record().canonical_bytes()
    );
}

#[test]
fn checksum_ignores_whitespace_and_key_order_but_not_content() {
    let record = sample_record();
    let json = record.to_json();

    // Reformatting the file does not break the checksum.
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let compact = serde_json::to_string(&value).expect("serialize");
    assert!(!compact.contains('\n'));
    assert_eq!(
        MetadataRecord::from_json(&compact).expect("compact parses"),
        record
    );

    // Nor does reordering keys: rebuild the object with keys reversed.
    let fields = value.as_object().expect("record is an object");
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    keys.reverse();
    let mut reordered = String::from("{");
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            reordered.push(',');
        }
        reordered.push_str(&format!(
            "\"{key}\": {}",
            serde_json::to_string(&fields[*key]).expect("serialize")
        ));
    }
    reordered.push('}');
    assert_eq!(
        MetadataRecord::from_json(&reordered).expect("reordered parses"),
        record
    );

    // But a changed value is caught, even one that is otherwise valid.
    let tampered = json.replace("\"size\": 10485760", "\"size\": 10485761");
    assert_ne!(tampered, json);
    assert!(matches!(
        MetadataRecord::from_json(&tampered),
        Err(RecordError::ChecksumMismatch { .. })
    ));

    // And a damaged checksum field is caught.
    let checksum_hex = record.checksum().to_hex();
    let mut flipped = checksum_hex.clone().into_bytes();
    flipped[0] = if flipped[0] == b'0' { b'1' } else { b'0' };
    let bad_checksum = json.replace(&checksum_hex, &String::from_utf8(flipped).expect("utf-8"));
    assert!(matches!(
        MetadataRecord::from_json(&bad_checksum),
        Err(RecordError::ChecksumMismatch { .. })
    ));

    // A record without a checksum field is not accepted.
    let mut without = value.clone();
    without.as_object_mut().expect("object").remove("checksum");
    let bare = serde_json::to_string(&without).expect("serialize");
    assert!(matches!(
        MetadataRecord::from_json(&bare),
        Err(RecordError::Json(_))
    ));
}
