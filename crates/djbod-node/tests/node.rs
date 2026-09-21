//! Tests of a running node serving the node-to-node operations, driven
//! through the client connection over localhost, with temporary
//! directories as devices.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use djbod_client::connection::{Connection, ConnectionError, StreamItem};
use djbod_core::checksum::checksum_block;
use djbod_core::cluster::{ClusterDocument, DeviceState};
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::hash_key;
use djbod_core::record::{
    DeviceId, MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME,
};
use djbod_core::stripe::{decode_stripe, encode_stripe, DecodedStripe, ShardBlock};
use djbod_core::version::VersionId;
use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_proto::handshake::{Hello, PeerKind, PROTOCOL_VERSION};
use djbod_proto::message::{ErrorCode, ListQuery, Request, Response, StreamEnd};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use uuid::Uuid;

const BLOCK: u64 = 64 * 1024; // the smallest block size the cluster document accepts

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

/// A running node with `device_count` devices, listening on localhost.
struct TestNode {
    node: Arc<Node>,
    addr: SocketAddr,
    _dirs: Vec<tempfile::TempDir>,
    _state: tempfile::TempDir,
}

async fn start_node(device_count: usize, k: u8, m: u8) -> TestNode {
    let dirs: Vec<tempfile::TempDir> = (0..device_count)
        .map(|_| tempfile::tempdir().expect("temp dir"))
        .collect();
    let state = tempfile::tempdir().expect("temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let config = NodeConfig {
        node_id: Uuid::new_v4(),
        listen: addr,
        advertise: None,
        state_dir: state.path().to_path_buf(),
        devices: dirs.iter().map(|d| d.path().to_path_buf()).collect(),
        bootstrap_peers: vec![],
        temporary_max_age_secs: 3600,
        stream_idle_timeout_secs: 120,
        // Temporary directories all live on one filesystem.
        allow_shared_filesystem: true,
        tls: None,
    };
    let parameters = ClusterParameters {
        k,
        m,
        block_size: BLOCK,
        headroom: 0.0,
        ..ClusterParameters::default()
    };
    let node = Arc::new(Node::init_cluster(config, parameters).expect("init cluster"));
    tokio::spawn(server::serve(node.clone(), listener));
    TestNode {
        node,
        addr,
        _dirs: dirs,
        _state: state,
    }
}

impl TestNode {
    fn devices(&self) -> Vec<DeviceId> {
        self.node.devices().iter().map(|d| d.id()).collect()
    }

    fn device_root(&self, id: DeviceId) -> PathBuf {
        self.node.device(id).expect("device").root().to_path_buf()
    }

    async fn connect_as_client(&self) -> Connection {
        Connection::connect(self.addr, Connection::client_hello(self.node.cluster_id()))
            .await
            .expect("connect")
    }

    async fn connect_as_node(&self) -> Connection {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            kind: PeerKind::Node,
            node_id: Some(djbod_core::cluster::NodeId(Uuid::new_v4())),
            cluster_id: self.node.cluster_id(),
            document_version: self.node.document_version(),
            build: None,
            cluster_name: None,
        };
        Connection::connect(self.addr, hello)
            .await
            .expect("connect as node")
    }
}

/// Encode `object` and return the blocks of every shard, indexed by
/// shard index, in stripe order.
fn encode_object(scheme: Scheme, object: &[u8]) -> Vec<Vec<ShardBlock>> {
    let code = ReedSolomonCode::new(scheme);
    let mut per_shard: Vec<Vec<ShardBlock>> = vec![Vec::new(); scheme.total_shards()];
    for stripe in object.chunks(scheme.data_shards() * BLOCK as usize) {
        let blocks = encode_stripe(&code, stripe, BLOCK as usize).expect("encode");
        for block in blocks {
            per_shard[block.index.as_usize()].push(block);
        }
    }
    per_shard
}

fn put_shard_request(
    device: DeviceId,
    key: &str,
    version: VersionId,
    scheme: Scheme,
    index: u8,
    object_size: u64,
) -> Request {
    Request::PutShard {
        device,
        key_hash: hash_key(key.as_bytes()),
        version,
        shard_index: index,
        k: scheme.data_shards() as u8,
        m: scheme.parity_shards() as u8,
        block_length: BLOCK,
        object_size,
    }
}

fn record_for(
    key: &str,
    version: VersionId,
    scheme: Scheme,
    object: &[u8],
    devices: &[DeviceId],
) -> MetadataRecord {
    MetadataRecord {
        format_version: RECORD_FORMAT_VERSION,
        system: SYSTEM_NAME.to_string(),
        bucket: "default".to_string(),
        key: key.to_string(),
        key_hash: hash_key(key.as_bytes()),
        version,
        created: OffsetDateTime::now_utc(),
        size: object.len() as u64,
        object_checksum: checksum_block(object),
        k: scheme.data_shards() as u8,
        m: scheme.parity_shards() as u8,
        block_size: BLOCK,
        shards: (0..scheme.total_shards())
            .map(|i| ShardLocation {
                index: i as u8,
                device: devices[i],
            })
            .collect(),
        content_type: None,
        user_metadata: BTreeMap::new(),
        revision: 0,
    }
}

/// Store a whole object on one node's devices through the protocol, one
/// shard per device, then its record on every holder.
async fn store_object(
    test: &TestNode,
    conn: &mut Connection,
    key: &str,
    version: VersionId,
    object: &[u8],
) -> MetadataRecord {
    let scheme = test.node.document().scheme().expect("scheme");
    let devices = test.devices();
    assert!(devices.len() >= scheme.total_shards());
    let per_shard = encode_object(scheme, object);
    let checksum = checksum_block(object);
    for (i, blocks) in per_shard.iter().enumerate() {
        conn.put_shard(
            put_shard_request(
                devices[i],
                key,
                version,
                scheme,
                i as u8,
                object.len() as u64,
            ),
            blocks,
            object.len() as u64,
            checksum,
        )
        .await
        .expect("put shard");
    }
    let record = record_for(key, version, scheme, object, &devices);
    for device in &devices[..scheme.total_shards()] {
        let response = conn
            .request(Request::PutMeta {
                device: *device,
                record: record.clone(),
            })
            .await
            .expect("put meta");
        assert_eq!(response, Response::PutMeta);
    }
    record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_exchange_and_local_status() {
    let test = start_node(3, 2, 1).await;
    let mut conn = test.connect_as_client().await;
    let peer = conn.peer_hello();
    assert_eq!(peer.kind, PeerKind::Node);
    assert_eq!(peer.node_id, Some(test.node.id()));
    assert_eq!(peer.cluster_id, test.node.cluster_id());
    assert_eq!(peer.document_version, 1);

    match conn.request(Request::LocalStatus).await.expect("status") {
        Response::LocalStatus {
            node,
            document_version,
            devices,
            ..
        } => {
            assert_eq!(node, test.node.id());
            assert_eq!(document_version, 1);
            assert_eq!(devices.len(), 3);
            for status in &devices {
                assert_eq!(status.node, test.node.id());
                assert_eq!(status.state, DeviceState::Active);
                assert!(status.total_bytes > 0);
                assert!(status.free_bytes > 0);
            }
        }
        other => panic!("expected LocalStatus, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_from_the_wrong_cluster_or_a_stale_node_is_refused() {
    let test = start_node(1, 1, 0).await;

    let wrong = Connection::connect(test.addr, Connection::client_hello(Uuid::new_v4())).await;
    match wrong {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::ProtocolViolation)
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    let stale = Hello {
        protocol_version: PROTOCOL_VERSION,
        kind: PeerKind::Node,
        node_id: Some(djbod_core::cluster::NodeId(Uuid::new_v4())),
        cluster_id: test.node.cluster_id(),
        document_version: 99,
        build: None,
        cluster_name: None,
    };
    match Connection::connect(test.addr, stale).await {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::DocumentVersionMismatch);
            assert_eq!(detail.node, Some(test.node.id()));
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    // A node peer with the right version is accepted.
    test.connect_as_node().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shards_and_records_round_trip_through_the_protocol() {
    let test = start_node(4, 3, 1).await;
    let mut conn = test.connect_as_node().await;
    let key = "docs/report.pdf";
    let version = VersionId::from_text("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("ulid");
    let object = xorshift64_bytes(5 * 3 * BLOCK as usize + 1234, 1);
    let record = store_object(&test, &mut conn, key, version, &object).await;

    // Files exist where the layout says, with no temporaries.
    for (i, device) in test.devices().iter().enumerate() {
        let dir = test
            .node
            .device(*device)
            .expect("device")
            .object_directory(&record.key_hash);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "device {i}: {names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".tmp")));
    }

    // Read the data shards back and decode every stripe intact.
    let scheme = record.scheme().expect("scheme");
    let code = ReedSolomonCode::new(scheme);
    let stripe_count = (object.len() as u64).div_ceil(scheme.data_shards() as u64 * BLOCK);
    let mut per_shard = Vec::new();
    for index in scheme.data_shard_indices() {
        let device = record.device_for(index).expect("device for index");
        let blocks = conn
            .get_shard(
                Request::GetShard {
                    device,
                    key_hash: record.key_hash,
                    version,
                    shard_index: index.0,
                    first_block: 0,
                    block_count: stripe_count,
                },
                index,
            )
            .await
            .expect("get shard");
        assert_eq!(blocks.len() as u64, stripe_count);
        per_shard.push(blocks);
    }
    let stripe_size = scheme.data_shards() * BLOCK as usize;
    let mut recovered = Vec::new();
    for stripe in 0..stripe_count as usize {
        let received: Vec<ShardBlock> = per_shard.iter().map(|s| s[stripe].clone()).collect();
        let start = stripe * stripe_size;
        let len = (object.len() - start).min(stripe_size);
        match decode_stripe(&code, &scheme.data_shard_indices(), &received, len).expect("decode") {
            DecodedStripe::Intact { data } => recovered.extend_from_slice(&data),
            other => panic!("stripe {stripe}: {other:?}"),
        }
    }
    assert_eq!(recovered, object);

    // A partial range read.
    let device0 = record.device_for(ShardIndex(0)).expect("device");
    let two = conn
        .get_shard(
            Request::GetShard {
                device: device0,
                key_hash: record.key_hash,
                version,
                shard_index: 0,
                first_block: 2,
                block_count: 2,
            },
            ShardIndex(0),
        )
        .await
        .expect("get shard range");
    assert_eq!(two.len(), 2);
    assert_eq!(two[0], per_shard[0][2]);
    assert_eq!(two[1], per_shard[0][3]);

    // Lookup finds the record on every device; GetMeta with probe sees the shard.
    match conn
        .request(Request::LocalLookup {
            key_hash: record.key_hash,
            after: None,
        })
        .await
        .expect("lookup")
    {
        Response::LocalLookup { records, .. } => {
            assert_eq!(records.len(), 4);
            for located in &records {
                assert_eq!(located.record, record);
            }
        }
        other => panic!("expected LocalLookup, got {other:?}"),
    }
    match conn
        .request(Request::GetMeta {
            device: device0,
            key_hash: record.key_hash,
            version,
            probe: true,
        })
        .await
        .expect("get meta")
    {
        Response::GetMeta {
            record: found,
            shard_present,
        } => {
            assert_eq!(found, Some(record.clone()));
            assert_eq!(shard_present, Some(true));
        }
        other => panic!("expected GetMeta, got {other:?}"),
    }
    match conn
        .request(Request::GetMeta {
            device: device0,
            key_hash: hash_key(b"missing"),
            version,
            probe: true,
        })
        .await
        .expect("get meta of missing")
    {
        Response::GetMeta {
            record: None,
            shard_present: Some(false),
        } => {}
        other => panic!("expected empty GetMeta, got {other:?}"),
    }

    // Delete from one device: its lookup copy is gone, the others remain.
    assert_eq!(
        conn.request(Request::DeleteVersion {
            device: device0,
            key_hash: record.key_hash,
            version,
        })
        .await
        .expect("delete"),
        Response::DeleteVersion
    );
    match conn
        .request(Request::LocalLookup {
            key_hash: record.key_hash,
            after: None,
        })
        .await
        .expect("lookup")
    {
        Response::LocalLookup { records, .. } => {
            assert_eq!(records.len(), 3);
            assert!(records.iter().all(|r| r.device != device0));
        }
        other => panic!("expected LocalLookup, got {other:?}"),
    }
    let gone = conn
        .get_shard(
            Request::GetShard {
                device: device0,
                key_hash: record.key_hash,
                version,
                shard_index: 0,
                first_block: 0,
                block_count: 1,
            },
            ShardIndex(0),
        )
        .await;
    match gone {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::NotFound);
            assert_eq!(detail.device, Some(device0));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_list_filters_sorts_deduplicates_and_paginates() {
    let test = start_node(2, 1, 1).await;
    let mut conn = test.connect_as_node().await;
    for (i, key) in ["b/2", "a/1", "b/1", "c"].iter().enumerate() {
        let object = xorshift64_bytes(100 + i, i as u64);
        store_object(&test, &mut conn, key, VersionId([i as u8 + 1; 16]), &object).await;
    }
    // Every record exists on both devices (1+1); the list has each once.
    match conn
        .request(Request::LocalList(ListQuery {
            prefix: None,
            start_after: None,
            limit: None,
        }))
        .await
        .expect("list")
    {
        Response::LocalList { entries, .. } => {
            let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(keys, vec!["a/1", "b/1", "b/2", "c"]);
            assert_eq!(entries[0].size, 101);
        }
        other => panic!("expected LocalList, got {other:?}"),
    }
    match conn
        .request(Request::LocalList(ListQuery {
            prefix: Some("b/".to_string()),
            start_after: Some("b/1".to_string()),
            limit: Some(5),
        }))
        .await
        .expect("list")
    {
        Response::LocalList { entries, .. } => {
            let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(keys, vec!["b/2"]);
        }
        other => panic!("expected LocalList, got {other:?}"),
    }
    match conn
        .request(Request::LocalList(ListQuery {
            prefix: None,
            start_after: None,
            limit: Some(2),
        }))
        .await
        .expect("list")
    {
        Response::LocalList { entries, .. } => assert_eq!(entries.len(), 2),
        other => panic!("expected LocalList, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_order_stripe_is_refused_and_leaves_no_file() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_node().await;
    let device = test.devices()[0];
    let scheme = Scheme::new(1, 0).expect("scheme");
    let object = xorshift64_bytes(3 * BLOCK as usize, 7);
    let blocks = &encode_object(scheme, &object)[0];
    let version = VersionId([9u8; 16]);

    let id = conn
        .send_request(put_shard_request(
            device,
            "k",
            version,
            scheme,
            0,
            object.len() as u64,
        ))
        .await
        .expect("send");
    assert_eq!(
        conn.read_response(id).await.expect("ready"),
        Response::PutShardReady
    );
    // Stripe 0 then stripe 2: the node refuses and closes.
    conn.send_data(
        id,
        djbod_proto::message::DataFrame {
            sequence: 0,
            checksum: blocks[0].checksum,
            bytes: blocks[0].bytes.clone(),
        },
    )
    .await
    .expect("send data");
    conn.send_data(
        id,
        djbod_proto::message::DataFrame {
            sequence: 2,
            checksum: blocks[2].checksum,
            bytes: blocks[2].bytes.clone(),
        },
    )
    .await
    .expect("send data");
    match conn.read_response(id).await {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::ProtocolViolation);
            assert!(detail
                .message
                .contains("stripe 2 arrived when stripe 1 was expected"));
        }
        other => panic!("expected ProtocolViolation, got {other:?}"),
    }
    // The connection is closed afterwards.
    assert!(conn.request(Request::LocalStatus).await.is_err());

    // Nothing remains on disk: no temporary, no shard file, and the key
    // directory is empty or absent.
    let dir = test
        .node
        .device(device)
        .expect("device")
        .object_directory(&hash_key(b"k"));
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shard_whose_blocks_do_not_match_the_object_size_is_refused() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_node().await;
    let device = test.devices()[0];
    let scheme = Scheme::new(1, 0).expect("scheme");
    let object = xorshift64_bytes(3 * BLOCK as usize, 8);
    let blocks = &encode_object(scheme, &object)[0];
    // Send only two of the three blocks the object size implies.
    let result = conn
        .put_shard(
            put_shard_request(
                device,
                "k",
                VersionId([10u8; 16]),
                scheme,
                0,
                object.len() as u64,
            ),
            &blocks[..2],
            object.len() as u64,
            checksum_block(&object),
        )
        .await;
    match result {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::WriteFailed);
            assert_eq!(detail.device, Some(device));
            assert!(
                detail.message.contains("needs 3 blocks"),
                "{}",
                detail.message
            );
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }
    let dir = test
        .node
        .device(device)
        .expect("device")
        .object_directory(&hash_key(b"k"));
    let leftovers = std::fs::read_dir(&dir).map(|e| e.count()).unwrap_or(0);
    assert_eq!(leftovers, 0);
    // The connection is still usable: this was a reported error, not a violation.
    conn.request(Request::LocalStatus)
        .await
        .expect("status after refusal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_corrupted_on_disk_is_returned_with_its_mismatching_checksum() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_node().await;
    let device = test.devices()[0];
    let scheme = Scheme::new(1, 0).expect("scheme");
    let object = xorshift64_bytes(2 * BLOCK as usize, 11);
    let blocks = &encode_object(scheme, &object)[0];
    let version = VersionId([11u8; 16]);
    conn.put_shard(
        put_shard_request(device, "k", version, scheme, 0, object.len() as u64),
        blocks,
        object.len() as u64,
        checksum_block(&object),
    )
    .await
    .expect("put shard");

    // Flip a byte inside block 1 on disk.
    let path = test.device_root(device).join("objects").join("default");
    let shard_path = walk_for_suffix(&path, ".shard").expect("shard file");
    let mut bytes = std::fs::read(&shard_path).expect("read");
    bytes[4096 + BLOCK as usize + 17] ^= 0x40;
    std::fs::write(&shard_path, &bytes).expect("write");

    let read = conn
        .get_shard(
            Request::GetShard {
                device,
                key_hash: hash_key(b"k"),
                version,
                shard_index: 0,
                first_block: 0,
                block_count: 2,
            },
            ShardIndex(0),
        )
        .await
        .expect("get shard");
    assert_eq!(checksum_block(&read[0].bytes), read[0].checksum);
    assert_ne!(checksum_block(&read[1].bytes), read[1].checksum);
    // The decoder turns that into an erasure, and with m = 0 cannot recover.
    let code = ReedSolomonCode::new(scheme);
    match decode_stripe(&code, &[ShardIndex(0)], &read[1..2], BLOCK as usize).expect("decode") {
        DecodedStripe::Unrecoverable { faults, .. } => assert_eq!(faults.len(), 1),
        other => panic!("expected Unrecoverable, got {other:?}"),
    }
}

fn walk_for_suffix(dir: &std::path::Path, suffix: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = walk_for_suffix(&path, suffix) {
                return Some(found);
            }
        } else if path.to_string_lossy().ends_with(suffix) {
            return Some(path);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_cluster_config_requires_a_higher_version_and_persists() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_node().await;
    let current: ClusterDocument = match conn.request(Request::GetClusterConfig).await.expect("get")
    {
        Response::GetClusterConfig { document } => document,
        other => panic!("expected GetClusterConfig, got {other:?}"),
    };
    assert_eq!(current.version, 1);

    // The same or a lower version is refused: nodes only move forward
    // (SPEC 6.2.6.1). A higher version is accepted even if it skips
    // numbers, which is how stragglers catch up.
    let same = current.clone();
    match conn
        .request(Request::ApplyClusterConfig { document: same })
        .await
    {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::DocumentVersionMismatch)
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    let mut other_cluster = current.clone();
    other_cluster.version = 2;
    other_cluster.cluster_id = Uuid::new_v4();
    match conn
        .request(Request::ApplyClusterConfig {
            document: other_cluster,
        })
        .await
    {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::DocumentVersionMismatch)
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    let mut next = current.clone();
    next.version = 2;
    next.devices[0].state = DeviceState::Draining;
    assert_eq!(
        conn.request(Request::ApplyClusterConfig {
            document: next.clone()
        })
        .await
        .expect("apply"),
        Response::ApplyClusterConfig
    );
    assert_eq!(test.node.document_version(), 2);
    let saved: ClusterDocument = serde_json::from_str(
        &std::fs::read_to_string(test._state.path().join("cluster.json")).expect("read"),
    )
    .expect("parse");
    assert_eq!(saved, next);

    // A draining device accepts no new shards.
    let device = test.devices()[0];
    let scheme = Scheme::new(1, 0).expect("scheme");
    let object = xorshift64_bytes(BLOCK as usize, 3);
    let blocks = &encode_object(scheme, &object)[0];
    match conn
        .put_shard(
            put_shard_request(
                device,
                "k",
                VersionId([12u8; 16]),
                scheme,
                0,
                object.len() as u64,
            ),
            blocks,
            object.len() as u64,
            checksum_block(&object),
        )
        .await
    {
        Err(ConnectionError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::WriteFailed);
            assert!(detail.message.contains("not active"));
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    // The old connection's Hello was for version 1; a fresh node
    // connection must present version 2.
    let stale = Hello {
        protocol_version: PROTOCOL_VERSION,
        kind: PeerKind::Node,
        node_id: Some(djbod_core::cluster::NodeId(Uuid::new_v4())),
        cluster_id: test.node.cluster_id(),
        document_version: 1,
        build: None,
        cluster_name: None,
    };
    assert!(matches!(
        Connection::connect(test.addr, stale).await,
        Err(ConnectionError::Remote(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sender_that_abandons_a_shard_leaves_nothing_and_abort_shard_is_idempotent() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_node().await;
    let device = test.devices()[0];
    let scheme = Scheme::new(1, 0).expect("scheme");
    let object = xorshift64_bytes(2 * BLOCK as usize, 5);
    let blocks = &encode_object(scheme, &object)[0];
    let version = VersionId([13u8; 16]);

    let id = conn
        .send_request(put_shard_request(
            device,
            "k",
            version,
            scheme,
            0,
            object.len() as u64,
        ))
        .await
        .expect("send");
    assert_eq!(
        conn.read_response(id).await.expect("ready"),
        Response::PutShardReady
    );
    conn.send_data(
        id,
        djbod_proto::message::DataFrame {
            sequence: 0,
            checksum: blocks[0].checksum,
            bytes: blocks[0].bytes.clone(),
        },
    )
    .await
    .expect("send data");
    conn.send_end(
        id,
        StreamEnd::failed(djbod_proto::message::ErrorDetail::new(
            ErrorCode::WriteFailed,
            "another holder failed",
        )),
    )
    .await
    .expect("send end");
    match conn.read_response(id).await {
        Err(ConnectionError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::WriteFailed),
        other => panic!("expected WriteFailed, got {other:?}"),
    }
    let dir = test
        .node
        .device(device)
        .expect("device")
        .object_directory(&hash_key(b"k"));
    let leftovers = std::fs::read_dir(&dir).map(|e| e.count()).unwrap_or(0);
    assert_eq!(leftovers, 0);

    // AbortShard on something that no longer exists succeeds.
    assert_eq!(
        conn.request(Request::AbortShard {
            device,
            key_hash: hash_key(b"k"),
            version,
            shard_index: 0,
        })
        .await
        .expect("abort"),
        Response::AbortShard
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_frame_between_operations_closes_the_connection() {
    let test = start_node(1, 1, 0).await;
    let mut conn = test.connect_as_client().await;
    conn.send_data(
        7,
        djbod_proto::message::DataFrame {
            sequence: 0,
            checksum: checksum_block(b""),
            bytes: vec![],
        },
    )
    .await
    .expect("send");
    let result = conn.request(Request::LocalStatus).await;
    assert!(
        result.is_err(),
        "expected the node to close the connection, got {result:?}"
    );
    let _ = StreamItem::End(StreamEnd::ok()); // keep the import used
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_devices_on_one_filesystem_are_refused_unless_allowed() {
    let a = tempfile::tempdir().expect("temp dir");
    let b = tempfile::tempdir().expect("temp dir");
    let state = tempfile::tempdir().expect("temp dir");
    let config = NodeConfig {
        node_id: Uuid::new_v4(),
        listen: "127.0.0.1:0".parse().expect("addr"),
        advertise: None,
        state_dir: state.path().to_path_buf(),
        devices: vec![a.path().to_path_buf(), b.path().to_path_buf()],
        bootstrap_peers: vec![],
        temporary_max_age_secs: 3600,
        stream_idle_timeout_secs: 120,
        allow_shared_filesystem: false,
        tls: None,
    };
    match Node::init_cluster(config, ClusterParameters::default()) {
        Err(djbod_node::node::NodeError::SameFilesystem { .. }) => {}
        Ok(_) => panic!("two directories on one filesystem should be refused"),
        Err(other) => panic!("expected SameFilesystem, got {other:?}"),
    }
}
