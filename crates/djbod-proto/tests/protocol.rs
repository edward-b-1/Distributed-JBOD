//! Tests of the wire protocol: frames, handshake, and every message.

use std::collections::BTreeMap;

use djbod_core::checksum::{checksum_block, BlockChecksum};
use djbod_core::cluster::{
    ClusterDocument, DeviceEntry, DeviceState, IndependenceLevel, NodeEntry, NodeId,
};
use djbod_core::keyhash::hash_key;
use djbod_core::record::{
    DeviceId, MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME,
};
use djbod_core::version::VersionId;
use djbod_proto::frame::{
    Frame, FrameError, FrameHeader, MessageType, HEADER_LEN, MAX_PAYLOAD_LEN,
};
use djbod_proto::handshake::{
    compute_proof, verify_proof, Hello, HelloProof, PeerKind, Role, PROTOCOL_VERSION,
};
use djbod_proto::message::{
    DataFrame, DeviceStatus, ErrorCode, ErrorDetail, KeyEntry, ListQuery, LocatedRecord, Message,
    MessageError, Request, Response, StreamEnd, DATA_PREFIX_LEN,
};
use time::macros::datetime;
use uuid::Uuid;

fn device(n: u128) -> DeviceId {
    DeviceId(Uuid::from_u128(0x1000 + n))
}

fn node(n: u128) -> NodeId {
    NodeId(Uuid::from_u128(0x2000 + n))
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
        size: 10 << 20,
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
    }
}

fn sample_document() -> ClusterDocument {
    ClusterDocument {
        version: 7,
        cluster_id: Uuid::from_u128(0xC1),
        k: 3,
        m: 1,
        block_size: 1 << 20,
        independence_level: IndependenceLevel::Device,
        headroom: 0.05,
        nodes: vec![NodeEntry {
            id: node(1),
            addresses: vec!["10.0.0.1:7000".to_string()],
        }],
        devices: vec![DeviceEntry {
            id: device(1),
            node: node(1),
            state: DeviceState::Active,
        }],
    }
}

fn round_trip(message: Message) -> usize {
    let bytes = message.encode().expect("failed to encode message");
    let (decoded, consumed) = Message::decode(&bytes).expect("failed to decode message");
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, message);
    bytes.len()
}

#[test]
fn frame_header_is_twelve_little_endian_bytes() {
    let header = FrameHeader {
        message_type: MessageType::Data,
        flags: 0,
        request_id: 0x0403_0201,
        payload_len: 0x0000_1000,
    };
    let bytes = header.encode();
    assert_eq!(bytes.len(), HEADER_LEN);
    assert_eq!(&bytes[0..2], &[5, 0]);
    assert_eq!(&bytes[2..4], &[0, 0]);
    assert_eq!(&bytes[4..8], &[1, 2, 3, 4]);
    assert_eq!(&bytes[8..12], &[0, 0x10, 0, 0]);
    assert_eq!(FrameHeader::decode(&bytes).expect("decode"), header);
}

#[test]
fn frame_decoding_reports_incomplete_with_the_total_needed() {
    let frame = Frame::new(MessageType::Request, 9, vec![1, 2, 3, 4, 5]);
    let bytes = frame.encode();
    assert_eq!(
        Frame::decode(&bytes[..4]),
        Err(FrameError::Incomplete {
            have: 4,
            needed: HEADER_LEN
        })
    );
    assert_eq!(
        Frame::decode(&bytes[..HEADER_LEN + 2]),
        Err(FrameError::Incomplete {
            have: HEADER_LEN + 2,
            needed: HEADER_LEN + 5
        })
    );
    // Trailing bytes belong to the next frame and are not consumed.
    let mut two = bytes.clone();
    two.extend_from_slice(&bytes);
    let (decoded, consumed) = Frame::decode(&two).expect("decode");
    assert_eq!(decoded, frame);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn frame_decoding_rejects_bad_headers_before_reading_payload() {
    let mut bytes = Frame::new(MessageType::Request, 1, vec![]).encode();
    bytes[0] = 99;
    assert_eq!(
        FrameHeader::decode(&bytes),
        Err(FrameError::UnknownMessageType(99))
    );

    let mut bytes = Frame::new(MessageType::Request, 1, vec![]).encode();
    bytes[2] = 1;
    assert_eq!(
        FrameHeader::decode(&bytes),
        Err(FrameError::UnknownFlags(1))
    );

    let mut bytes = Frame::new(MessageType::Data, 1, vec![]).encode();
    let too_big = MAX_PAYLOAD_LEN + 1;
    bytes[8..12].copy_from_slice(&too_big.to_le_bytes());
    assert_eq!(
        FrameHeader::decode(&bytes),
        Err(FrameError::PayloadTooLarge(too_big))
    );
    // A header claiming an enormous payload is rejected without waiting
    // for the payload, so a hostile peer cannot make us allocate it.
    assert_eq!(
        Frame::decode(&bytes),
        Err(FrameError::PayloadTooLarge(too_big))
    );
}

#[test]
fn data_frame_costs_exactly_sixteen_bytes_over_the_block() {
    let block = vec![0xABu8; 1 << 20];
    let data = DataFrame {
        sequence: 42,
        checksum: checksum_block(&block),
        bytes: block.clone(),
    };
    let wire = round_trip(Message::Data {
        id: 3,
        data: data.clone(),
    });
    assert_eq!(wire, HEADER_LEN + DATA_PREFIX_LEN + block.len());

    let payload = data.encode_payload();
    assert_eq!(&payload[0..8], &42u64.to_le_bytes());
    assert_eq!(&payload[8..16], &data.checksum.0.to_le_bytes());
    assert_eq!(&payload[16..], &block[..]);

    assert!(matches!(
        DataFrame::decode_payload(&payload[..10]),
        Err(MessageError::DataFrameTooShort(10))
    ));
    // An empty chunk is legal: prefix only.
    let empty = DataFrame {
        sequence: 0,
        checksum: checksum_block(&[]),
        bytes: vec![],
    };
    assert_eq!(
        DataFrame::decode_payload(&empty.encode_payload()).expect("decode"),
        empty
    );
}

#[test]
fn handshake_proofs_verify_only_with_the_right_secret_nonces_role_and_cluster() {
    let secret = b"correct horse battery staple";
    let initiator_nonce = [1u8; 32];
    let responder_nonce = [2u8; 32];
    let cluster = Uuid::from_u128(0xC1);

    let proof = compute_proof(
        secret,
        Role::Initiator,
        &initiator_nonce,
        &responder_nonce,
        cluster,
    );
    assert!(verify_proof(
        secret,
        Role::Initiator,
        &initiator_nonce,
        &responder_nonce,
        cluster,
        &proof
    ));

    assert!(!verify_proof(
        b"wrong secret",
        Role::Initiator,
        &initiator_nonce,
        &responder_nonce,
        cluster,
        &proof
    ));
    assert!(!verify_proof(
        secret,
        Role::Responder,
        &initiator_nonce,
        &responder_nonce,
        cluster,
        &proof
    ));
    assert!(!verify_proof(
        secret,
        Role::Initiator,
        &[9u8; 32],
        &responder_nonce,
        cluster,
        &proof
    ));
    assert!(!verify_proof(
        secret,
        Role::Initiator,
        &initiator_nonce,
        &[9u8; 32],
        cluster,
        &proof
    ));
    assert!(!verify_proof(
        secret,
        Role::Initiator,
        &initiator_nonce,
        &responder_nonce,
        Uuid::from_u128(0xC2),
        &proof
    ));

    let mut tampered = proof;
    tampered[0] ^= 1;
    assert!(!verify_proof(
        secret,
        Role::Initiator,
        &initiator_nonce,
        &responder_nonce,
        cluster,
        &tampered
    ));

    // The two roles produce different proofs, so one cannot be reflected.
    let responder_proof = compute_proof(
        secret,
        Role::Responder,
        &initiator_nonce,
        &responder_nonce,
        cluster,
    );
    assert_ne!(proof, responder_proof);
}

#[test]
fn handshake_messages_round_trip_as_byte_strings() {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        kind: PeerKind::Node,
        node_id: Some(node(1)),
        cluster_id: Uuid::from_u128(0xC1),
        document_version: 7,
        nonce: [0x5Au8; 32],
    };
    let wire = round_trip(Message::Hello(hello.clone()));
    // The 32-byte nonce is a CBOR byte string (major type 2, one-byte
    // length 0x58 0x20) followed by the raw bytes: 34 bytes on the wire,
    // not 64 hex characters or a 32-element array.
    let bytes = Message::Hello(hello.clone()).encode().expect("encode");
    let mut expected = vec![0x58u8, 0x20];
    expected.extend_from_slice(&[0x5Au8; 32]);
    assert!(
        bytes
            .windows(expected.len())
            .any(|w| w == expected.as_slice()),
        "nonce not encoded as a byte string"
    );
    assert!(wire < HEADER_LEN + 256, "hello is {wire} bytes");

    let client = Hello {
        kind: PeerKind::Client,
        node_id: None,
        document_version: 0,
        ..hello
    };
    round_trip(Message::Hello(client));
    round_trip(Message::HelloProof(HelloProof { proof: [7u8; 32] }));
}

#[test]
fn every_request_round_trips() {
    let key_hash = hash_key(b"k");
    let version = VersionId([3u8; 16]);
    let requests = vec![
        Request::Status,
        Request::PutObject {
            key: "photos/2026/cat.jpg".to_string(),
            size: 12345,
            content_type: Some("image/jpeg".to_string()),
            user_metadata: BTreeMap::from([("album".to_string(), "cats".to_string())]),
        },
        Request::PutObject {
            key: "k".to_string(),
            size: 0,
            content_type: None,
            user_metadata: BTreeMap::new(),
        },
        Request::GetObject {
            key: "k".to_string(),
        },
        Request::HeadObject {
            key: "k".to_string(),
        },
        Request::DeleteObject {
            key: "k".to_string(),
        },
        Request::ListKeys(ListQuery {
            prefix: Some("photos/".to_string()),
            start_after: None,
            limit: Some(100),
        }),
        Request::LocalStatus,
        Request::LocalLookup { key_hash },
        Request::LocalList(ListQuery {
            prefix: None,
            start_after: Some("a".to_string()),
            limit: None,
        }),
        Request::PutShard {
            device: device(1),
            key_hash,
            version,
            shard_index: 2,
            k: 3,
            m: 1,
            block_length: 1 << 20,
            object_size: 10 << 20,
        },
        Request::GetShard {
            device: device(1),
            key_hash,
            version,
            shard_index: 2,
            first_block: 0,
            block_count: 4,
        },
        Request::PutMeta {
            device: device(1),
            record: sample_record(),
        },
        Request::GetMeta {
            device: device(1),
            key_hash,
            version,
            probe: true,
        },
        Request::DeleteVersion {
            device: device(1),
            key_hash,
            version,
        },
        Request::AbortShard {
            device: device(1),
            key_hash,
            version,
            shard_index: 2,
        },
        Request::GetClusterConfig,
        Request::ApplyClusterConfig {
            document: sample_document(),
        },
    ];
    for (i, request) in requests.into_iter().enumerate() {
        round_trip(Message::Request {
            id: i as u32,
            request,
        });
    }
}

#[test]
fn every_response_round_trips() {
    let status = DeviceStatus {
        device: device(1),
        node: node(1),
        state: DeviceState::Active,
        total_bytes: 4 << 40,
        free_bytes: 3 << 40,
    };
    let entry = KeyEntry {
        key: "k".to_string(),
        size: 5,
        version: VersionId([1u8; 16]),
    };
    let responses = vec![
        Response::Error(ErrorDetail {
            code: ErrorCode::BlockChecksumMismatch,
            message: "shard 2 stripe 4 failed its checksum".to_string(),
            node: Some(node(1)),
            device: Some(device(2)),
            key: Some("k".to_string()),
            version: Some(VersionId([1u8; 16])),
            shard_index: Some(2),
            stripe: Some(4),
        }),
        Response::Error(ErrorDetail::new(ErrorCode::NotFound, "no such key")),
        Response::Status {
            cluster_id: Uuid::from_u128(0xC1),
            document_version: 7,
            coordinator: node(1),
            devices: vec![status.clone()],
        },
        Response::PutObject {
            version: VersionId([1u8; 16]),
        },
        Response::GetObject {
            record: sample_record(),
        },
        Response::HeadObject {
            record: sample_record(),
        },
        Response::DeleteObject,
        Response::ListKeys {
            keys: vec![entry.clone()],
            truncated: false,
        },
        Response::LocalStatus {
            node: node(1),
            document_version: 7,
            devices: vec![status],
        },
        Response::LocalLookup {
            records: vec![LocatedRecord {
                device: device(1),
                record: sample_record(),
            }],
        },
        Response::LocalList {
            entries: vec![entry],
        },
        Response::PutShardReady,
        Response::PutShardDone,
        Response::GetShard { block_count: 4 },
        Response::PutMeta,
        Response::GetMeta {
            record: Some(sample_record()),
            shard_present: Some(true),
        },
        Response::GetMeta {
            record: None,
            shard_present: None,
        },
        Response::DeleteVersion,
        Response::AbortShard,
        Response::GetClusterConfig {
            document: sample_document(),
        },
        Response::ApplyClusterConfig,
    ];
    for (i, response) in responses.into_iter().enumerate() {
        round_trip(Message::Response {
            id: i as u32,
            response,
        });
    }
}

#[test]
fn stream_end_round_trips_in_all_three_shapes() {
    round_trip(Message::EndOfStream {
        id: 1,
        end: StreamEnd::ok(),
    });
    round_trip(Message::EndOfStream {
        id: 1,
        end: StreamEnd::failed(ErrorDetail::new(
            ErrorCode::ObjectChecksumMismatch,
            "whole-object checksum differs",
        )),
    });
    round_trip(Message::EndOfStream {
        id: 1,
        end: StreamEnd {
            error: None,
            object_size: Some(10 << 20),
            object_checksum: Some(BlockChecksum(5)),
        },
    });
    // The success case is tiny.
    let ok = Message::EndOfStream {
        id: 1,
        end: StreamEnd::ok(),
    }
    .encode()
    .expect("encode");
    assert!(
        ok.len() <= HEADER_LEN + 4,
        "StreamEnd::ok is {} bytes",
        ok.len()
    );
}

#[test]
fn request_ids_are_carried_and_handshake_frames_have_none() {
    let request = Message::Request {
        id: 77,
        request: Request::Status,
    };
    assert_eq!(request.request_id(), Some(77));
    let bytes = request.encode().expect("encode");
    assert_eq!(&bytes[4..8], &77u32.to_le_bytes());
    let hello = Message::HelloProof(HelloProof { proof: [0u8; 32] });
    assert_eq!(hello.request_id(), None);
}

#[test]
fn a_malformed_control_payload_is_a_codec_error_not_a_panic() {
    let frame = Frame::new(MessageType::Request, 1, vec![0xFF, 0x00, 0x12]);
    assert!(matches!(
        Message::from_frame(&frame),
        Err(MessageError::Codec(_))
    ));
    // Valid CBOR of the wrong shape.
    let frame = Frame::new(MessageType::Response, 1, vec![0x01]); // integer 1
    assert!(matches!(
        Message::from_frame(&frame),
        Err(MessageError::Codec(_))
    ));
}
