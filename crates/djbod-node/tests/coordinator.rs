//! Tests of the client-facing operations against a cluster of one node,
//! driven through the client connection: the milestone 2 goal.

use std::net::SocketAddr;
use std::sync::Arc;

use djbod_core::checksum::checksum_block;
use djbod_core::cluster::DeviceState;
use djbod_core::keyhash::hash_key;
use djbod_core::layout::shard_file_name;
use djbod_core::record::DeviceId;
use djbod_node::client::{ClientError, Connection};
use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_proto::message::{ErrorCode, ListQuery, Request, Response};
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
    };
    let parameters = ClusterParameters {
        k,
        m,
        block_size: BLOCK,
        headroom: 0.0,
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
