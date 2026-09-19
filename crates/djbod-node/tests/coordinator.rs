//! Tests of the client-facing operations against a cluster of one node,
//! driven through the client connection: the milestone 2 goal.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use djbod_core::checksum::checksum_block;
use djbod_core::cluster::DeviceState;
use djbod_core::erasure::ShardIndex;
use djbod_core::keyhash::hash_key;
use djbod_core::layout::shard_file_name;
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::version::VersionId;
use djbod_node::client::{ClientError, Connection};
use djbod_node::config::NodeConfig;
use djbod_node::membership;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_node::transport::Connector;
use djbod_proto::message::{
    ClusterFinding, DrainEvent, ErrorCode, ListQuery, Request, Response, ScrubEvent, ShardCondition,
};
use tokio::net::TcpListener;
use uuid::Uuid;

const BLOCK: u64 = 64 * 1024;
const CHUNK: usize = 100_000; // body chunk size clients use in these tests; not block aligned on purpose

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
    async fn client(&self) -> Connection {
        Connection::connect(self.addr, Connection::client_hello(self.node.cluster_id()))
            .await
            .expect("connect")
    }

    fn shard_path(
        &self,
        device: DeviceId,
        key: &str,
        record: &djbod_core::record::MetadataRecord,
    ) -> std::path::PathBuf {
        let index = record.shard_on(device).expect("device holds a shard");
        self.node
            .device(device)
            .expect("device")
            .object_directory(&hash_key(key.as_bytes()))
            .join(shard_file_name(&record.version, index))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_head_get_list_delete_round_trip() {
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;

    // Status shows every device active with room.
    match client.request(Request::Status).await.expect("status") {
        Response::Status {
            devices,
            coordinator,
            document_version,
            ..
        } => {
            assert_eq!(devices.len(), 4);
            assert_eq!(coordinator, test.node.id());
            assert_eq!(document_version, 1);
            assert!(devices
                .iter()
                .all(|d| d.state == DeviceState::Active && d.free_bytes > 0));
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Objects of assorted sizes: several stripes plus a partial, exactly
    // one stripe, smaller than one block, one byte, and empty.
    let stripe = 3 * BLOCK as usize;
    let sizes = [5 * stripe + 12_345, stripe, 1000, 1, 0];
    let mut objects = Vec::new();
    for (i, size) in sizes.iter().enumerate() {
        let key = format!("data/object-{i}");
        let body = xorshift64_bytes(*size, i as u64 + 1);
        let version = client
            .put_object(
                &key,
                &body,
                CHUNK,
                Some("application/octet-stream".to_string()),
            )
            .await
            .expect("put");
        objects.push((key, body, version));
    }

    for (key, body, version) in &objects {
        match client
            .request(Request::HeadObject { key: key.clone() })
            .await
            .expect("head")
        {
            Response::HeadObject { record } => {
                assert_eq!(record.key, *key);
                assert_eq!(record.version, *version);
                assert_eq!(record.size, body.len() as u64);
                assert_eq!(record.object_checksum, checksum_block(body));
                assert_eq!(
                    record.content_type.as_deref(),
                    Some("application/octet-stream")
                );
                assert_eq!(record.shards.len(), 4);
                // Four distinct devices.
                let mut devices: Vec<DeviceId> = record.shards.iter().map(|s| s.device).collect();
                devices.sort();
                devices.dedup();
                assert_eq!(devices.len(), 4);
            }
            other => panic!("expected HeadObject, got {other:?}"),
        }
        let (record, got) = client.get_object(key).await.expect("get");
        assert_eq!(record.version, *version);
        assert_eq!(&got, body, "body mismatch for {key}");
    }

    // Listing: sorted, one entry per key, paginated.
    match client
        .request(Request::ListKeys(ListQuery {
            prefix: Some("data/".to_string()),
            start_after: None,
            limit: Some(3),
        }))
        .await
        .expect("list")
    {
        Response::ListKeys { keys, truncated } => {
            let names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(
                names,
                vec!["data/object-0", "data/object-1", "data/object-2"]
            );
            assert!(truncated);
            assert_eq!(keys[1].size, stripe as u64);
        }
        other => panic!("expected ListKeys, got {other:?}"),
    }
    match client
        .request(Request::ListKeys(ListQuery {
            prefix: None,
            start_after: Some("data/object-2".to_string()),
            limit: None,
        }))
        .await
        .expect("list")
    {
        Response::ListKeys { keys, truncated } => {
            let names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(names, vec!["data/object-3", "data/object-4"]);
            assert!(!truncated);
        }
        other => panic!("expected ListKeys, got {other:?}"),
    }

    // Delete one; it is gone from head, get, and list; deleting again is NotFound.
    assert_eq!(
        client
            .request(Request::DeleteObject {
                key: "data/object-1".to_string()
            })
            .await
            .expect("delete"),
        Response::DeleteObject
    );
    for request in [
        Request::HeadObject {
            key: "data/object-1".to_string(),
        },
        Request::DeleteObject {
            key: "data/object-1".to_string(),
        },
    ] {
        match client.request(request).await {
            Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::NotFound),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
    match client.get_object("data/object-1").await {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::NotFound),
        other => panic!("expected NotFound, got {other:?}"),
    }
    match client
        .request(Request::ListKeys(ListQuery {
            prefix: None,
            start_after: None,
            limit: None,
        }))
        .await
        .expect("list")
    {
        Response::ListKeys { keys, .. } => assert_eq!(keys.len(), 4),
        other => panic!("expected ListKeys, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_replaces_the_previous_version_and_removes_it() {
    let test = start_node(2, 1, 1).await;
    let mut client = test.client().await;
    let first = xorshift64_bytes(3 * BLOCK as usize, 1);
    let second = xorshift64_bytes(BLOCK as usize + 7, 2);
    let v1 = client
        .put_object("k", &first, CHUNK, None)
        .await
        .expect("put 1");
    let record1 = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };
    let v2 = client
        .put_object("k", &second, CHUNK, None)
        .await
        .expect("put 2");
    assert!(v2 > v1, "versions must increase");

    let (record2, body) = client.get_object("k").await.expect("get");
    assert_eq!(record2.version, v2);
    assert_eq!(body, second);

    // The old version's files are gone from every device; only v2 remains.
    for device in test.node.devices() {
        let dir = device.object_directory(&hash_key(b"k"));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().all(|n| n.starts_with(&v2.to_text())),
            "{names:?}"
        );
        assert!(!test.shard_path(device.id(), "k", &record1).exists());
    }
    match client
        .request(Request::ListKeys(ListQuery {
            prefix: None,
            start_after: None,
            limit: None,
        }))
        .await
        .expect("list")
    {
        Response::ListKeys { keys, .. } => {
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].version, v2);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_block_fails_the_read_and_names_the_device() {
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(4 * 3 * BLOCK as usize, 9);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let record = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };

    // Flip a byte in stripe 2 of data shard 1.
    let device = record
        .device_for(djbod_core::erasure::ShardIndex(1))
        .expect("device");
    let path = test.shard_path(device, "k", &record);
    let mut bytes = std::fs::read(&path).expect("read");
    let offset = 4096 + 2 * BLOCK as usize + 100;
    bytes[offset] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");

    // Fail-stop: the stream ends with an error naming the device, shard,
    // and stripe, and no reconstruction is served (11.4). The bytes for
    // stripes 0 and 1 may have been delivered before the failure.
    match client.get_object("k").await {
        Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::BlockChecksumMismatch);
            assert_eq!(detail.device, Some(device));
            assert_eq!(detail.shard_index, Some(1));
            assert_eq!(detail.stripe, Some(2));
            assert_eq!(detail.key.as_deref(), Some("k"));
        }
        other => panic!("expected StreamFailed, got {other:?}"),
    }
    // Head still works: the record copies are intact.
    let mut client = test.client().await;
    client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_record_copy_fails_lookups_as_inconsistent() {
    let test = start_node(2, 1, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(BLOCK as usize, 4);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");

    // Change one byte of one record copy on disk.
    let device = test.node.devices()[0].clone();
    let dir = device.object_directory(&hash_key(b"k"));
    let record_path = std::fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .find(|p| p.to_string_lossy().ends_with(".meta.json"))
        .expect("record file");
    let text = std::fs::read_to_string(&record_path).expect("read");
    std::fs::write(
        &record_path,
        text.replace("\"size\": 65536", "\"size\": 65537"),
    )
    .expect("write");

    match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::RecordsInconsistent);
            assert_eq!(detail.device, Some(device.id()));
        }
        other => panic!("expected RecordsInconsistent, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insufficient_devices_refuses_the_write_before_any_body_is_stored() {
    // 3+1 needs four devices; the cluster has three.
    let test = start_node(3, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(BLOCK as usize, 4);
    match client.put_object("k", &body, CHUNK, None).await {
        Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::InsufficientDevices);
        }
        other => panic!("expected InsufficientDevices, got {other:?}"),
    }
    for device in test.node.devices() {
        assert!(!device.object_directory(&hash_key(b"k")).exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_shorter_or_longer_than_declared_is_refused_and_leaves_nothing() {
    let test = start_node(2, 1, 1).await;
    for (declared, actual) in [(1000u64, 999usize), (1000, 1001)] {
        let mut client = test.client().await;
        let body = xorshift64_bytes(actual, 5);
        let id = client
            .send_request(Request::PutObject {
                key: "k".to_string(),
                size: declared,
                content_type: None,
                user_metadata: Default::default(),
            })
            .await
            .expect("send");
        client
            .send_data(id, djbod_node::client::body_frame(0, body))
            .await
            .expect("data");
        client
            .send_end(id, djbod_proto::message::StreamEnd::ok())
            .await
            .expect("end");
        // The refusal arrives as a failed stream end, then the connection
        // is closed.
        match client.read_stream_item(id).await {
            Ok(djbod_node::client::StreamItem::End(end)) => {
                let error = end.error.expect("stream end carries the error");
                assert_eq!(error.code, ErrorCode::ProtocolViolation);
            }
            other => panic!("expected a failed stream end, got {other:?}"),
        }
        assert!(
            client.request(Request::Status).await.is_err(),
            "connection should be closed"
        );
        for device in test.node.devices() {
            let dir = device.object_directory(&hash_key(b"k"));
            let count = std::fs::read_dir(&dir).map(|e| e.count()).unwrap_or(0);
            assert_eq!(count, 0, "declared {declared} actual {actual}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_keys_and_empty_keys_are_refused() {
    let test = start_node(1, 1, 0).await;
    let mut client = test.client().await;
    let long = "x".repeat(16 * 1024 + 1);
    match client.request(Request::HeadObject { key: long }).await {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::KeyTooLong),
        other => panic!("expected KeyTooLong, got {other:?}"),
    }
    match client
        .request(Request::HeadObject { key: String::new() })
        .await
    {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::ProtocolViolation),
        other => panic!("expected ProtocolViolation, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replication_and_jbod_schemes_work_too() {
    // k = 1: pure replication; m = 0: unprotected JBOD.
    for (devices, k, m) in [(3usize, 1u8, 2u8), (2, 2, 0)] {
        let test = start_node(devices, k, m).await;
        let mut client = test.client().await;
        let body = xorshift64_bytes(2 * BLOCK as usize + 3, 6);
        client
            .put_object("k", &body, CHUNK, None)
            .await
            .expect("put");
        let (record, got) = client.get_object("k").await.expect("get");
        assert_eq!(got, body);
        assert_eq!(record.k, k);
        assert_eq!(record.m, m);
    }
}

async fn repair(client: &mut Connection, key: &str) -> djbod_proto::message::RepairReport {
    match client
        .request(Request::RepairObject {
            key: key.to_string(),
        })
        .await
        .expect("repair")
    {
        Response::RepairObject(report) => report,
        other => panic!("expected RepairObject, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_rewrites_a_corrupt_shard_and_the_object_reads_again() {
    use djbod_proto::message::ShardCondition;
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(5 * 3 * BLOCK as usize + 999, 21);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let record = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };

    // Nothing to do on an intact object.
    let report = repair(&mut client, "k").await;
    assert!(report
        .shards
        .iter()
        .all(|s| s.condition == ShardCondition::Intact && !s.rewritten));

    // Corrupt two blocks of data shard 2.
    let device = record
        .device_for(djbod_core::erasure::ShardIndex(2))
        .expect("device");
    let path = test.shard_path(device, "k", &record);
    let before = std::fs::read(&path).expect("read");
    let mut bytes = before.clone();
    bytes[4096 + BLOCK as usize + 7] ^= 0x01; // stripe 1
    bytes[4096 + 4 * BLOCK as usize + 7] ^= 0x01; // stripe 4
    std::fs::write(&path, &bytes).expect("write");
    assert!(matches!(
        client.get_object("k").await,
        Err(ClientError::StreamFailed(_))
    ));
    // A failed stream closes the connection; open another.
    let mut client = test.client().await;

    let report = repair(&mut client, "k").await;
    let shard2 = report
        .shards
        .iter()
        .find(|s| s.index == 2)
        .expect("shard 2");
    assert_eq!(
        shard2.condition,
        ShardCondition::CorruptBlocks {
            stripes: vec![1, 4]
        }
    );
    assert!(shard2.rewritten);
    assert_eq!(report.shards.iter().filter(|s| s.rewritten).count(), 1);
    // The rewritten file is byte-for-byte what was there before the damage.
    assert_eq!(std::fs::read(&path).expect("read"), before);
    let (_, got) = client.get_object("k").await.expect("get after repair");
    assert_eq!(got, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_recreates_a_missing_or_structurally_broken_shard() {
    use djbod_proto::message::ShardCondition;
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(2 * 3 * BLOCK as usize, 22);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let record = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };

    // Delete the parity shard's file entirely.
    let parity_device = record
        .device_for(djbod_core::erasure::ShardIndex(3))
        .expect("device");
    let parity_path = test.shard_path(parity_device, "k", &record);
    let parity_before = std::fs::read(&parity_path).expect("read");
    std::fs::remove_file(&parity_path).expect("remove");

    // Append a byte to data shard 0, as a text editor would.
    let data_device = record
        .device_for(djbod_core::erasure::ShardIndex(0))
        .expect("device");
    let data_path = test.shard_path(data_device, "k", &record);
    let data_before = std::fs::read(&data_path).expect("read");
    let mut extended = data_before.clone();
    extended.push(b'\n');
    std::fs::write(&data_path, &extended).expect("write");

    // Two damaged shards with m = 1 is beyond repair.
    match client
        .request(Request::RepairObject {
            key: "k".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::BlockChecksumMismatch)
        }
        other => panic!("expected refusal, got {other:?}"),
    }
    // Neither file was touched.
    assert!(!parity_path.exists());
    assert_eq!(std::fs::read(&data_path).expect("read"), extended);

    // Restore the parity file; now one shard is damaged and repair succeeds.
    std::fs::write(&parity_path, &parity_before).expect("restore");
    let report = repair(&mut client, "k").await;
    let shard0 = report
        .shards
        .iter()
        .find(|s| s.index == 0)
        .expect("shard 0");
    assert!(matches!(
        shard0.condition,
        ShardCondition::Unreadable { .. }
    ));
    assert!(shard0.rewritten);
    assert_eq!(std::fs::read(&data_path).expect("read"), data_before);
    assert_eq!(client.get_object("k").await.expect("get").1, body);

    // And a missing file alone is recreated identically.
    std::fs::remove_file(&parity_path).expect("remove");
    let report = repair(&mut client, "k").await;
    let shard3 = report
        .shards
        .iter()
        .find(|s| s.index == 3)
        .expect("shard 3");
    assert!(matches!(
        shard3.condition,
        ShardCondition::Unreadable { .. }
    ));
    assert!(shard3.rewritten);
    assert_eq!(std::fs::read(&parity_path).expect("read"), parity_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_of_a_missing_key_and_an_empty_object() {
    let test = start_node(2, 1, 1).await;
    let mut client = test.client().await;
    match client
        .request(Request::RepairObject {
            key: "nothing".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::NotFound),
        other => panic!("expected NotFound, got {other:?}"),
    }
    client
        .put_object("empty", &[], CHUNK, None)
        .await
        .expect("put");
    let report = repair(&mut client, "empty").await;
    assert_eq!(report.shards.len(), 2);
    assert!(report.shards.iter().all(|s| !s.rewritten));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_rewrites_a_missing_record_copy_and_refuses_when_fewer_than_k_remain() {
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(2 * 3 * BLOCK as usize, 31);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let record = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };
    let record_path = |device: DeviceId| {
        test.node
            .device(device)
            .expect("device")
            .object_directory(&hash_key(b"k"))
            .join(djbod_core::layout::record_file_name(&record.version))
    };

    // Delete one record copy: reads refuse (9.4.4), repair rewrites it.
    let victim = record.shards[1].device;
    let before = std::fs::read(record_path(victim)).expect("read");
    std::fs::remove_file(record_path(victim)).expect("remove");
    match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::RecordsInconsistent)
        }
        other => panic!("expected RecordsInconsistent, got {other:?}"),
    }
    let report = repair(&mut client, "k").await;
    assert_eq!(report.record_copies_rewritten, vec![victim]);
    assert!(
        report.shards.iter().all(|s| !s.rewritten),
        "shards were intact"
    );
    assert_eq!(std::fs::read(record_path(victim)).expect("read"), before);
    client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head after repair");

    // Delete two of four: only two remain, fewer than k = 3; repair refuses.
    std::fs::remove_file(record_path(record.shards[0].device)).expect("remove");
    std::fs::remove_file(record_path(record.shards[2].device)).expect("remove");
    match client
        .request(Request::RepairObject {
            key: "k".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::RecordsInconsistent);
            assert!(
                detail.message.contains("fewer than k"),
                "{}",
                detail.message
            );
        }
        other => panic!("expected refusal, got {other:?}"),
    }
}

async fn run_scrub(client: &mut Connection, repair: bool) -> Vec<ScrubEvent> {
    let id = client.start_scrub(None, repair).await.expect("start scrub");
    let mut events = Vec::new();
    loop {
        match client.next_scrub_event(id).await.expect("scrub event") {
            Ok(event) => events.push(event),
            Err(end) => {
                assert!(end.error.is_none(), "{end:?}");
                return events;
            }
        }
    }
}

fn no_findings(events: &[ScrubEvent]) -> bool {
    !events.iter().any(|e| {
        matches!(
            e,
            ScrubEvent::NodeFinding { .. } | ScrubEvent::ClusterFinding(_)
        )
    })
}

fn record_path(test: &TestNode, device: DeviceId, key: &str, version: &VersionId) -> PathBuf {
    test.node
        .device(device)
        .expect("device")
        .object_directory(&hash_key(key.as_bytes()))
        .join(djbod_core::layout::record_file_name(version))
}

fn spare_device(test: &TestNode, record: &MetadataRecord) -> DeviceId {
    test.node
        .devices()
        .into_iter()
        .map(|d| d.id())
        .find(|d| record.shard_on(*d).is_none())
        .expect("one device holds nothing")
}

#[allow(clippy::result_large_err)]
async fn head(client: &mut Connection, key: &str) -> Result<MetadataRecord, ClientError> {
    match client
        .request(Request::HeadObject {
            key: key.to_string(),
        })
        .await?
    {
        Response::HeadObject { record } => Ok(record),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_shard_relocates_the_shard_and_raises_the_record_revision() {
    let test = start_node(5, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(2 * 3 * BLOCK as usize + 17, 41);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let before = head(&mut client, "k").await.expect("head");
    assert_eq!(before.revision, 0);
    let spare = spare_device(&test, &before);
    let source = before.shards[1].device;
    let old_shard = std::fs::read(test.shard_path(source, "k", &before)).expect("read shard");
    let old_record =
        std::fs::read(record_path(&test, source, "k", &before.version)).expect("read record");

    // A holder is not an eligible destination.
    match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 1,
            target: Some(before.shards[0].device),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::InsufficientDevices)
        }
        other => panic!("expected InsufficientDevices, got {other:?}"),
    }

    // Automatic choice: the only device that holds nothing.
    let after = match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 1,
            target: None,
        })
        .await
        .expect("move")
    {
        Response::MoveShard {
            record,
            source: reported_source,
            source_cleaned,
            rebuilt,
        } => {
            assert_eq!(reported_source, source);
            assert!(source_cleaned);
            assert!(
                !rebuilt,
                "the source was intact and should have been copied"
            );
            record
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(after.revision, 1);
    assert_eq!(after.device_for(ShardIndex(1)), Some(spare));
    assert!(after.same_body(&before));
    assert_eq!(
        std::fs::read(test.shard_path(spare, "k", &after)).expect("moved shard"),
        old_shard
    );
    assert!(!test.shard_path(source, "k", &before).exists());
    assert!(!record_path(&test, source, "k", &before.version).exists());
    for shard in &after.shards {
        let copy = MetadataRecord::from_json(
            &std::fs::read_to_string(record_path(&test, shard.device, "k", &after.version))
                .expect("record copy"),
        )
        .expect("parse");
        assert_eq!(copy, after);
    }
    assert_eq!(head(&mut client, "k").await.expect("head"), after);
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
    let events = run_scrub(&mut client, false).await;
    assert!(no_findings(&events), "{events:?}");

    // An interrupted re-placement: one holder still has the revision 0
    // copy. Reads fail until repair finishes the move forwards (18.8.1).
    let lagging = after.shards[0].device;
    std::fs::write(
        record_path(&test, lagging, "k", &after.version),
        &old_record,
    )
    .expect("write");
    match head(&mut client, "k").await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::RecordsInconsistent);
            assert!(
                detail.message.contains("3 record copies found, 4 expected"),
                "{}",
                detail.message
            );
        }
        other => panic!("expected RecordsInconsistent, got {other:?}"),
    }
    let report = repair(&mut client, "k").await;
    assert_eq!(report.record_copies_rewritten, vec![lagging]);
    assert!(report.stale_copies_removed.is_empty());
    assert!(report.shards.iter().all(|s| !s.rewritten));
    assert_eq!(head(&mut client, "k").await.expect("head"), after);

    // A stale copy: the source comes back with its old record and shard.
    // Reads ignore it, the scrub reports it, repair removes it.
    std::fs::create_dir_all(
        test.shard_path(source, "k", &before)
            .parent()
            .expect("key directory"),
    )
    .expect("mkdir");
    std::fs::write(test.shard_path(source, "k", &before), &old_shard).expect("write shard");
    std::fs::write(
        record_path(&test, source, "k", &before.version),
        &old_record,
    )
    .expect("write");
    assert_eq!(head(&mut client, "k").await.expect("head"), after);
    let events = run_scrub(&mut client, false).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScrubEvent::ClusterFinding(ClusterFinding::StaleCopy {
                key,
                device,
                revision: 0,
                current_revision: 1,
                ..
            }) if key == "k" && *device == source
        )),
        "{events:?}"
    );
    let report = repair(&mut client, "k").await;
    assert_eq!(report.stale_copies_removed, vec![source]);
    assert!(!test.shard_path(source, "k", &before).exists());
    assert!(!record_path(&test, source, "k", &before.version).exists());
    let events = run_scrub(&mut client, false).await;
    assert!(no_findings(&events), "{events:?}");

    // The cleaned source is free again, so an automatic choice lands there
    // and the revision keeps counting.
    match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 0,
            target: None,
        })
        .await
        .expect("move")
    {
        Response::MoveShard { record, .. } => {
            assert_eq!(record.revision, 2);
            assert_eq!(record.device_for(ShardIndex(0)), Some(source));
        }
        other => panic!("{other:?}"),
    }
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_shard_rebuilds_from_the_other_shards_when_the_source_is_damaged() {
    let test = start_node(5, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(3 * 3 * BLOCK as usize, 42);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let before = head(&mut client, "k").await.expect("head");
    let source = before.shards[2].device;
    let spare = spare_device(&test, &before);

    // Flip a byte in the second block of the source shard: the copy fails
    // on that block's checksum and the move falls back to a rebuild.
    let path = test.shard_path(source, "k", &before);
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[4096 + BLOCK as usize + 5] ^= 0x80;
    std::fs::write(&path, &bytes).expect("write");

    let after = match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 2,
            target: Some(spare),
        })
        .await
        .expect("move")
    {
        Response::MoveShard {
            record,
            source_cleaned,
            rebuilt,
            ..
        } => {
            assert!(rebuilt);
            assert!(source_cleaned);
            record
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(after.revision, 1);
    assert_eq!(after.device_for(ShardIndex(2)), Some(spare));
    assert!(!path.exists());
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
    let events = run_scrub(&mut client, false).await;
    assert!(no_findings(&events), "{events:?}");
}

async fn set_state(test: &TestNode, device: DeviceId, state: DeviceState) -> bool {
    let (_, changed) = membership::set_device_state(
        &Connector::plain(),
        test.addr,
        test.node.cluster_id(),
        device,
        state,
    )
    .await
    .expect("set state");
    changed
}

async fn run_drain(
    client: &mut Connection,
    device: DeviceId,
    partial: bool,
) -> (Vec<DrainEvent>, djbod_proto::message::StreamEnd) {
    let id = client
        .start_drain(device, partial)
        .await
        .expect("start drain");
    let mut events = Vec::new();
    loop {
        match client.next_drain_event(id).await.expect("drain event") {
            Ok(event) => events.push(event),
            Err(end) => return (events, end),
        }
    }
}

fn device_state(statuses: &[djbod_proto::message::DeviceStatus], device: DeviceId) -> DeviceState {
    statuses
        .iter()
        .find(|d| d.device == device)
        .expect("device listed")
        .state
}

async fn status_devices(client: &mut Connection) -> Vec<djbod_proto::message::DeviceStatus> {
    match client.request(Request::Status).await.expect("status") {
        Response::Status { devices, .. } => devices,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_state_changes_placement_and_nothing_else() {
    let test = start_node(5, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(3 * BLOCK as usize, 51);
    client
        .put_object("a", &body, CHUNK, None)
        .await
        .expect("put");
    let a = head(&mut client, "a").await.expect("head");
    let device = a.shards[0].device;

    assert!(set_state(&test, device, DeviceState::Draining).await);
    assert_eq!(
        device_state(&status_devices(&mut client).await, device),
        DeviceState::Draining
    );
    // Its shard is untouched and still serves reads.
    assert!(test.shard_path(device, "a", &a).exists());
    let (_, got) = client.get_object("a").await.expect("get");
    assert_eq!(got, body);
    // New placements avoid it: the other four devices are the only choice.
    client
        .put_object("b", &body, CHUNK, None)
        .await
        .expect("put");
    let b = head(&mut client, "b").await.expect("head");
    assert!(b.shard_on(device).is_none(), "{b:?}");
    // So does re-placement.
    match client
        .request(Request::MoveShard {
            key: "b".to_string(),
            shard_index: 0,
            target: Some(device),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::InsufficientDevices)
        }
        other => panic!("expected InsufficientDevices, got {other:?}"),
    }
    // Draining is not required for a drain-less life: set it back.
    assert!(set_state(&test, device, DeviceState::Active).await);
    assert!(!set_state(&test, device, DeviceState::Active).await);
    assert_eq!(
        device_state(&status_devices(&mut client).await, device),
        DeviceState::Active
    );
    match membership::set_device_state(
        &Connector::plain(),
        test.addr,
        test.node.cluster_id(),
        DeviceId(Uuid::new_v4()),
        DeviceState::Draining,
    )
    .await
    {
        Err(membership::MembershipError::UnknownDevice(_)) => {}
        other => panic!("expected UnknownDevice, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_moves_every_version_off_the_device_and_reports_stale_copies() {
    let test = start_node(5, 3, 1).await;
    let mut client = test.client().await;
    let mut bodies = Vec::new();
    for i in 0..3u64 {
        let body = xorshift64_bytes(2 * 3 * BLOCK as usize + i as usize * 1000, 60 + i);
        client
            .put_object(&format!("obj-{i}"), &body, CHUNK, None)
            .await
            .expect("put");
        bodies.push(body);
    }
    // Every device holds at least one shard: three 3+1 writes over five
    // devices, most-free-first, cannot leave one out. Pick obj-0's first.
    let obj0 = head(&mut client, "obj-0").await.expect("head");
    let device = obj0.shards[0].device;
    let on_device = |records: &[MetadataRecord]| {
        records
            .iter()
            .filter(|r| r.shard_on(device).is_some())
            .count()
    };
    let mut records = Vec::new();
    for i in 0..3 {
        records.push(head(&mut client, &format!("obj-{i}")).await.expect("head"));
    }
    let expected_versions = on_device(&records);
    let old_shard = std::fs::read(test.shard_path(device, "obj-0", &obj0)).expect("read");
    let old_record =
        std::fs::read(record_path(&test, device, "obj-0", &obj0.version)).expect("read");

    // An active device cannot be drained: the state change is a separate,
    // explicit step.
    match client.start_drain(device, false).await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::ProtocolViolation);
            assert!(detail.message.contains("set-state"), "{}", detail.message);
        }
        other => panic!("expected refusal, got {other:?}"),
    }
    set_state(&test, device, DeviceState::Draining).await;

    let (events, end) = run_drain(&mut client, device, false).await;
    assert!(end.error.is_none(), "{end:?}");
    match &events[0] {
        DrainEvent::Estimate {
            versions,
            active_devices,
            required_devices,
            shard_bytes,
            ..
        } => {
            assert_eq!(*versions as usize, expected_versions);
            assert_eq!(*active_devices, 4);
            assert_eq!(*required_devices, 4);
            assert!(*shard_bytes > 0);
        }
        other => panic!("expected Estimate first, got {other:?}"),
    }
    let moved: Vec<&DrainEvent> = events
        .iter()
        .filter(|e| matches!(e, DrainEvent::Moved { .. }))
        .collect();
    assert_eq!(moved.len(), expected_versions);
    assert!(!events
        .iter()
        .any(|e| matches!(e, DrainEvent::Skipped { .. })));
    for event in &moved {
        if let DrainEvent::Moved {
            destination,
            rebuilt,
            ..
        } = event
        {
            assert_ne!(*destination, device);
            assert!(!rebuilt);
        }
    }
    // The device is empty, every object reads, the records moved on by one
    // revision, and the scrub is clean.
    let key_dir = test.shard_path(device, "obj-0", &obj0);
    assert!(!key_dir.parent().expect("key dir").exists());
    for (i, body) in bodies.iter().enumerate() {
        let key = format!("obj-{i}");
        let (_, got) = client.get_object(&key).await.expect("get");
        assert_eq!(&got, body);
        let record = head(&mut client, &key).await.expect("head");
        assert!(record.shard_on(device).is_none());
        assert_eq!(
            record.revision,
            u64::from(records[i].shard_on(device).is_some())
        );
    }
    let events = run_scrub(&mut client, false).await;
    assert!(no_findings(&events), "{events:?}");
    // A second pass finds nothing to do.
    let (events, end) = run_drain(&mut client, device, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert_eq!(events.len(), 1, "{events:?}");

    // A stale copy left on the device is reported, not moved, and the
    // drain ends with an error naming it.
    std::fs::create_dir_all(key_dir.parent().expect("key dir")).expect("mkdir");
    std::fs::write(&key_dir, &old_shard).expect("write");
    std::fs::write(
        record_path(&test, device, "obj-0", &obj0.version),
        &old_record,
    )
    .expect("write");
    let (events, end) = run_drain(&mut client, device, false).await;
    assert_eq!(
        end.error.as_ref().map(|e| e.code),
        Some(ErrorCode::WriteFailed),
        "{end:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            DrainEvent::Skipped { key, detail, .. }
                if key == "obj-0" && detail.message.contains("stale copy")
        )),
        "{events:?}"
    );
    assert!(key_dir.exists());
    let (_, got) = client.get_object("obj-0").await.expect("get");
    assert_eq!(got, bodies[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_refuses_without_room_unless_partial_and_then_skips_what_cannot_move() {
    let test = start_node(4, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(3 * BLOCK as usize, 70);
    client
        .put_object("a", &body, CHUNK, None)
        .await
        .expect("put");
    let a = head(&mut client, "a").await.expect("head");
    let device = a.shards[3].device;
    set_state(&test, device, DeviceState::Draining).await;

    // Three active devices cannot hold a 3+1 version: refused after the
    // estimate, nothing moved.
    let (events, end) = run_drain(&mut client, device, false).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(
        &events[0],
        DrainEvent::Estimate {
            versions: 1,
            active_devices: 3,
            required_devices: 4,
            ..
        }
    ));
    let error = end.error.expect("refused");
    assert_eq!(error.code, ErrorCode::InsufficientDevices);
    assert!(error.message.contains("--partial"), "{}", error.message);
    assert_eq!(head(&mut client, "a").await.expect("head"), a);

    // With --partial the pass runs and every version is skipped for want
    // of a target; the device keeps serving.
    let (events, end) = run_drain(&mut client, device, true).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            DrainEvent::Skipped { key, detail, .. }
                if key == "a" && detail.code == ErrorCode::InsufficientDevices
        )),
        "{events:?}"
    );
    assert_eq!(
        end.error.as_ref().map(|e| e.code),
        Some(ErrorCode::WriteFailed)
    );
    assert_eq!(head(&mut client, "a").await.expect("head"), a);
    let (_, got) = client.get_object("a").await.expect("get");
    assert_eq!(got, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_rebuilds_the_shards_of_a_device_that_left_the_document() {
    let test = start_node(5, 3, 1).await;
    let mut client = test.client().await;
    let body = xorshift64_bytes(2 * 3 * BLOCK as usize + 9, 80);
    client
        .put_object("k", &body, CHUNK, None)
        .await
        .expect("put");
    let before = head(&mut client, "k").await.expect("head");
    let lost = before.shards[2].device;
    let spare = spare_device(&test, &before);

    // The device leaves the document (as a forced removal does, 6.2.6.3).
    let mut next = test.node.document();
    next.version += 1;
    next.devices.retain(|d| d.id != lost);
    test.node.apply_document(next).expect("apply");
    match client.get_object("k").await {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::DeviceUnavailable, "{detail:?}")
        }
        other => panic!("expected DeviceUnavailable, got {other:?}"),
    }

    // Repair rebuilds the lost shard onto the spare device and moves the
    // record on by one revision (18.3).
    let report = repair(&mut client, "k").await;
    let shard = report
        .shards
        .iter()
        .find(|s| s.index == 2)
        .expect("shard 2");
    assert_eq!(shard.device, lost);
    assert_eq!(shard.condition, ShardCondition::Lost);
    assert!(shard.rewritten);
    assert_eq!(shard.relocated_to, Some(spare));
    assert!(report
        .shards
        .iter()
        .filter(|s| s.index != 2)
        .all(|s| s.condition == ShardCondition::Intact && s.relocated_to.is_none()));
    assert_eq!(report.record_copies_rewritten[0], spare);
    assert_eq!(report.record_copies_rewritten.len(), 4);

    let after = head(&mut client, "k").await.expect("head");
    assert_eq!(after.revision, 1);
    assert_eq!(after.device_for(ShardIndex(2)), Some(spare));
    assert!(test.shard_path(spare, "k", &after).exists());
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
    // Nothing more to do: a second repair finds every shard intact.
    let report = repair(&mut client, "k").await;
    assert!(report.shards.iter().all(|s| !s.rewritten));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_limits_come_from_the_cluster_document() {
    let test = start_node(2, 1, 1).await;
    let mut client = test.client().await;
    let mut next = test.node.document();
    next.version += 1;
    next.max_key_bytes = 8;
    next.max_object_bytes = 1000;
    test.node.apply_document(next).expect("apply");

    match client.put_object("short", &[7u8; 1000], CHUNK, None).await {
        Ok(_) => {}
        other => panic!("1000 bytes is within the limit: {other:?}"),
    }
    match client.put_object("short", &[7u8; 1001], CHUNK, None).await {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::ObjectTooLarge);
            assert!(detail.message.contains("1000"), "{}", detail.message);
        }
        other => panic!("expected ObjectTooLarge, got {other:?}"),
    }
    // A refused PUT closes the connection once the body starts arriving.
    let mut client = test.client().await;
    match client
        .put_object("nine-long", &[7u8; 10], CHUNK, None)
        .await
    {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::KeyTooLong);
            assert!(detail.message.contains("limit is 8"), "{}", detail.message);
        }
        other => panic!("expected KeyTooLong, got {other:?}"),
    }
    let mut client = test.client().await;
    match client
        .request(Request::HeadObject {
            key: "nine-long".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::KeyTooLong),
        other => panic!("expected KeyTooLong, got {other:?}"),
    }
    // The object stored under the old limits is untouched.
    let (_, got) = client.get_object("short").await.expect("get");
    assert_eq!(got, vec![7u8; 1000]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_metadata_is_refused_before_any_body_is_stored() {
    use djbod_core::record::MAX_CONTENT_TYPE_BYTES;
    const MAX_USER_METADATA_BYTES: usize = 4096;
    let test = start_node(2, 1, 1).await;
    let mut client = test.client().await;
    // The user metadata limit is the document's; make it small here.
    let mut next = test.node.document();
    next.version += 1;
    next.max_user_metadata_bytes = MAX_USER_METADATA_BYTES as u64;
    test.node.apply_document(next).expect("apply");
    let body = [1u8; 10];
    let long_type = Some("x".repeat(MAX_CONTENT_TYPE_BYTES + 1));
    match client.put_object("k", &body, CHUNK, long_type).await {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::MetadataTooLarge, "{detail:?}")
        }
        other => panic!("expected MetadataTooLarge, got {other:?}"),
    }
    let mut client = test.client().await;
    let mut big = std::collections::BTreeMap::new();
    big.insert("blob".to_string(), "y".repeat(MAX_USER_METADATA_BYTES - 3));
    let mut cursor: &[u8] = &body;
    match client
        .put_object_with_metadata("k", 10, &mut cursor, CHUNK, None, big)
        .await
    {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::MetadataTooLarge, "{detail:?}")
        }
        other => panic!("expected MetadataTooLarge, got {other:?}"),
    }
    for device in test.node.devices() {
        assert!(!device.object_directory(&hash_key(b"k")).exists());
    }
    // At the limit is fine, and the metadata comes back.
    let mut client = test.client().await;
    let mut fits = std::collections::BTreeMap::new();
    fits.insert("blob".to_string(), "y".repeat(MAX_USER_METADATA_BYTES - 4));
    let mut cursor: &[u8] = &body;
    client
        .put_object_with_metadata(
            "k",
            10,
            &mut cursor,
            CHUNK,
            Some("x".repeat(MAX_CONTENT_TYPE_BYTES)),
            fits.clone(),
        )
        .await
        .expect("put at the limits");
    let record = head(&mut client, "k").await.expect("head");
    assert_eq!(record.user_metadata, fits);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listings_are_paged_so_no_response_outgrows_a_frame() {
    use djbod_proto::message::{RecordCursor, MAX_LIST_PAGE_BYTES};
    // One device, no parity: every record and shard is written once, which
    // keeps this test cheap, since a debug build serializes 1 MiB keys
    // slowly.
    let test = start_node(1, 1, 0).await;
    let mut client = test.client().await;
    // Ten keys at the largest length a document may allow, 1 MiB each:
    // 10 MiB of key text, more than one page.
    let mut next = test.node.document();
    next.version += 1;
    next.max_key_bytes = djbod_core::cluster::LIMIT_MAX_KEY_BYTES;
    test.node.apply_document(next).expect("apply");
    let key_len = djbod_core::cluster::LIMIT_MAX_KEY_BYTES as usize;
    let count = 10u32;
    for i in 0..count {
        let key = format!("{i:06}-") + &"k".repeat(key_len - 7);
        client
            .put_object(&key, b"x", CHUNK, None)
            .await
            .expect("put");
    }

    // A client listing with no limit gets a frame-sized page and a
    // truncation flag; paging through start_after yields every key once.
    let mut seen: Vec<String> = Vec::new();
    let mut start_after: Option<String> = None;
    let mut pages = 0;
    loop {
        match client
            .request(Request::ListKeys(ListQuery {
                prefix: None,
                start_after: start_after.clone(),
                limit: None,
            }))
            .await
            .expect("list")
        {
            Response::ListKeys { keys, truncated } => {
                pages += 1;
                let bytes: usize = keys.iter().map(|k| k.key.len()).sum();
                assert!(bytes <= MAX_LIST_PAGE_BYTES, "page of {bytes} bytes");
                start_after = keys.last().map(|k| k.key.clone());
                seen.extend(keys.into_iter().map(|k| k.key));
                if !truncated {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(pages >= 2, "{pages} page(s)");
    assert_eq!(seen.len(), count as usize);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, seen, "sorted and free of duplicates");

    // A small limit walks the same keys in more pages, and a limit of zero
    // still advances.
    for limit in [3u32, 0] {
        let mut walked: Vec<String> = Vec::new();
        let mut start_after: Option<String> = None;
        let mut pages = 0;
        loop {
            match client
                .request(Request::ListKeys(ListQuery {
                    prefix: None,
                    start_after: start_after.clone(),
                    limit: Some(limit),
                }))
                .await
                .expect("list")
            {
                Response::ListKeys { keys, truncated } => {
                    pages += 1;
                    assert!(!keys.is_empty(), "a page never comes back empty");
                    assert!(keys.len() <= limit.max(1) as usize);
                    start_after = keys.last().map(|k| k.key.clone());
                    walked.extend(keys.into_iter().map(|k| k.key));
                    if !truncated {
                        break;
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(walked, seen, "limit {limit}");
        assert!(pages >= (count / limit.max(1)) as usize, "{pages} page(s)");
    }

    // The per-device record listing pages the same way.
    let device = test.node.devices()[0].id();
    let mut records = 0usize;
    let mut after: Option<RecordCursor> = None;
    let mut pages = 0;
    loop {
        match client
            .request(Request::LocalRecords {
                device,
                after: after.clone(),
            })
            .await
            .expect("local records")
        {
            Response::LocalRecords {
                records: page,
                truncated,
            } => {
                pages += 1;
                records += page.len();
                after = page.last().map(|r| RecordCursor {
                    key: r.key.clone(),
                    version: r.version,
                });
                if !truncated {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(pages >= 2, "{pages} page(s)");
    assert_eq!(records, count as usize);

    // Everything that walks the whole key space still sees all of it.
    let events = run_scrub(&mut client, false).await;
    assert!(no_findings(&events), "{events:?}");
}
