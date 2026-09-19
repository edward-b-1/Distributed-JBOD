//! Several nodes in one process on localhost ports: joining, placement
//! across nodes, document changes, stragglers and sync, startup adoption,
//! and fail-stop when a node is down.

use std::net::SocketAddr;
use std::sync::Arc;

use djbod_core::cluster::{ClusterDocument, DeviceState};
use djbod_core::record::DeviceId;
use djbod_node::client::{ClientError, Connection};
use djbod_node::config::NodeConfig;
use djbod_node::membership;
use djbod_node::node::{ClusterParameters, Node, NodeError};
use djbod_node::server;
use djbod_node::transport::Connector;
use djbod_node::wire;
use djbod_proto::frame::{Frame, MessageType};
use djbod_proto::handshake::{Hello, PeerKind, PROTOCOL_VERSION};
use djbod_proto::message::{DrainEvent, ErrorCode, Message, Request, Response, ShardCondition};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use uuid::Uuid;

const BLOCK: u64 = 64 * 1024;

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

/// One node: its config, its serving task, and its directories.
struct TestNode {
    config: NodeConfig,
    node: Arc<Node>,
    addr: SocketAddr,
    server: Option<JoinHandle<()>>,
    _dirs: Vec<tempfile::TempDir>,
    _state: tempfile::TempDir,
}

fn make_config(
    device_count: usize,
    listen: SocketAddr,
    bootstrap: Vec<String>,
) -> (NodeConfig, Vec<tempfile::TempDir>, tempfile::TempDir) {
    let dirs: Vec<tempfile::TempDir> = (0..device_count)
        .map(|_| tempfile::tempdir().expect("temp dir"))
        .collect();
    let state = tempfile::tempdir().expect("temp dir");
    let config = NodeConfig {
        node_id: Uuid::new_v4(),
        listen,
        advertise: None,
        state_dir: state.path().to_path_buf(),
        devices: dirs.iter().map(|d| d.path().to_path_buf()).collect(),
        bootstrap_peers: bootstrap,
        temporary_max_age_secs: 3600,
        stream_idle_timeout_secs: 120,
        allow_shared_filesystem: true,
        tls: None,
    };
    (config, dirs, state)
}

async fn reserve_port() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// Create a cluster from one node.
async fn first_node(device_count: usize, k: u8, m: u8) -> TestNode {
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(device_count, addr, vec![]);
    let node = Arc::new(
        Node::init_cluster(
            config.clone(),
            ClusterParameters {
                k,
                m,
                block_size: BLOCK,
                headroom: 0.0,
                ..ClusterParameters::default()
            },
        )
        .expect("init cluster"),
    );
    let server = tokio::spawn(server::serve(node.clone(), listener));
    TestNode {
        config,
        node,
        addr,
        server: Some(server),
        _dirs: dirs,
        _state: state,
    }
}

/// Join a new node through `peer`, then start serving it.
async fn joined_node(device_count: usize, peer: &TestNode) -> TestNode {
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(device_count, addr, vec![peer.addr.to_string()]);
    membership::join(&config, peer.addr, peer.node.cluster_id(), false)
        .await
        .expect("join");
    let node = Arc::new(Node::open(config.clone()).expect("open joined node"));
    let server = tokio::spawn(server::serve(node.clone(), listener));
    TestNode {
        config,
        node,
        addr,
        server: Some(server),
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

    /// Stop serving: abort the accept loop and drop the listener.
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_join_and_objects_spread_across_them() {
    let a = first_node(2, 3, 1).await;
    let b = joined_node(2, &a).await;
    let c = joined_node(2, &b).await;

    // Every node holds version 3 listing three nodes and six devices.
    for n in [&a, &b, &c] {
        let document = n.node.document();
        assert_eq!(document.version, 3);
        assert_eq!(document.nodes.len(), 3);
        assert_eq!(document.devices.len(), 6);
        assert!(document
            .devices
            .iter()
            .all(|d| d.state == DeviceState::Active));
    }

    // Status through any node sees all six devices grouped by node.
    let mut client = c.client().await;
    match client.request(Request::Status).await.expect("status") {
        Response::Status {
            devices,
            document_version,
            ..
        } => {
            assert_eq!(document_version, 3);
            assert_eq!(devices.len(), 6);
        }
        other => panic!("{other:?}"),
    }

    // An object written through one node lands on devices of at least two
    // nodes and reads back through every node.
    let body = xorshift64_bytes(3 * 3 * BLOCK as usize + 77, 1);
    client
        .put_object("k", &body, 100_000, None)
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
    let document = c.node.document();
    let mut owners: Vec<_> = record
        .shards
        .iter()
        .map(|s| document.device(s.device).expect("device").node)
        .collect();
    owners.sort();
    owners.dedup();
    assert!(
        owners.len() >= 2,
        "four shards on two devices per node must span nodes: {owners:?}"
    );
    for n in [&a, &b, &c] {
        let mut client = n.client().await;
        let (_, got) = client.get_object("k").await.expect("get via each node");
        assert_eq!(got, body);
    }

    // `cluster show` data: every node reports version 3.
    let reports = membership::fetch_all(&Connector::plain(), &document).await;
    assert_eq!(reports.len(), 3);
    assert!(reports
        .iter()
        .all(|r| r.result.as_ref().map(|d| d.version) == Ok(3)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_node_fails_requests_with_its_name_and_resumes_after_restart() {
    let a = first_node(2, 2, 1).await;
    let mut b = joined_node(2, &a).await;
    let mut client = a.client().await;
    let body = xorshift64_bytes(2 * 2 * BLOCK as usize, 2);
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");

    b.stop();
    // Every operation that must reach b fails, naming it (16.1, 16.2).
    match client.request(Request::Status).await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::NodeUnreachable);
            assert_eq!(detail.node, Some(b.node.id()));
        }
        other => panic!("expected NodeUnreachable, got {other:?}"),
    }
    match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::NodeUnreachable),
        other => panic!("expected NodeUnreachable, got {other:?}"),
    }
    match client.put_object("k2", &body, 100_000, None).await {
        Err(ClientError::StreamFailed(detail)) => {
            assert_eq!(detail.code, ErrorCode::NodeUnreachable)
        }
        other => panic!("expected NodeUnreachable, got {other:?}"),
    }
    // Nothing was written for k2 on a.
    for device in a.node.devices() {
        assert!(!device
            .object_directory(&djbod_core::keyhash::hash_key(b"k2"))
            .exists());
    }

    // Restart b on the same address: everything works again.
    let listener = TcpListener::bind(b.addr).await.expect("rebind");
    let node = Arc::new(Node::open(b.config.clone()).expect("reopen"));
    b.server = Some(tokio::spawn(server::serve(node.clone(), listener)));
    let mut client = a.client().await;
    let (_, got) = client.get_object("k").await.expect("get after restart");
    assert_eq!(got, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_proposals_are_serialised_and_stragglers_are_synced() {
    let a = first_node(1, 1, 1).await;
    let b = joined_node(1, &a).await;
    let current = a.node.document();
    assert_eq!(current.version, 2);

    // Two proposals built from the same version: the second is refused by
    // the first-listed node and touches nothing.
    let mut first = current.clone();
    first.version = 3;
    first.headroom = 0.10;
    let mut second = current.clone();
    second.version = 3;
    second.headroom = 0.20;
    membership::propose(&Connector::plain(), &current, &first)
        .await
        .expect("first proposal");
    match membership::propose(&Connector::plain(), &current, &second).await {
        Err(membership::MembershipError::StaleProposal {
            expected: 2,
            found: 3,
        }) => {}
        Err(membership::MembershipError::Superseded { .. }) => {}
        other => panic!("expected the second proposal to lose, got {other:?}"),
    }
    assert_eq!(a.node.document().headroom, 0.10);
    assert_eq!(b.node.document().headroom, 0.10);

    // Make a straggler by applying version 4 to a alone.
    let mut fourth = a.node.document();
    fourth.version = 4;
    fourth.headroom = 0.30;
    a.node.apply_document(fourth.clone()).expect("apply to a");
    assert_eq!(a.node.document_version(), 4);
    assert_eq!(b.node.document_version(), 3);

    // Requests between them now fail (6.2.7), and a proposal is refused
    // because versions differ.
    let mut client = a.client().await;
    match client.request(Request::Status).await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::DocumentVersionMismatch)
        }
        other => panic!("expected DocumentVersionMismatch, got {other:?}"),
    }
    let mut fifth = fourth.clone();
    fifth.version = 5;
    assert!(matches!(
        membership::propose(&Connector::plain(), &fourth, &fifth).await,
        Err(membership::MembershipError::VersionsDiffer(_))
    ));

    // Sync brings b up; requests work again.
    let report = membership::sync(&Connector::plain(), b.addr, a.node.cluster_id())
        .await
        .expect("sync");
    assert_eq!(report.highest_version, 4);
    assert_eq!(report.updated, vec![b.node.id()]);
    assert_eq!(report.already_current, vec![a.node.id()]);
    assert!(report.unreachable.is_empty());
    assert_eq!(b.node.document_version(), 4);
    let mut client = a.client().await;
    client
        .request(Request::Status)
        .await
        .expect("status after sync");

    // A lower or equal version is refused everywhere.
    assert!(matches!(
        b.node.apply_document(fourth),
        Err(djbod_node::node::NodeError::NotNewer {
            current: 4,
            proposed: 4
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarting_node_adopts_a_newer_document_from_its_bootstrap_peer() {
    let a = first_node(1, 1, 1).await;
    let mut b = joined_node(1, &a).await;
    b.stop();

    // While b is down, the document moves on (applied to a alone, as a
    // change that could not reach b would leave it).
    let mut next = a.node.document();
    next.version += 1;
    next.headroom = 0.25;
    a.node.apply_document(next.clone()).expect("apply");

    // b starts: adoption from its bootstrap peer (a) brings it to the
    // newer version before it opens.
    let adopted = membership::adopt_from_peers(&b.config)
        .await
        .expect("adopt");
    assert_eq!(adopted.version, next.version);
    let saved: ClusterDocument = serde_json::from_str(
        &std::fs::read_to_string(Node::document_path_for(&b.config)).expect("read"),
    )
    .expect("parse");
    assert_eq!(saved.version, next.version);
    let reopened = Node::open(b.config.clone()).expect("open");
    assert_eq!(reopened.document_version(), next.version);

    // A peer from another cluster is an error; an unreachable peer is not.
    let mut lonely = b.config.clone();
    lonely.bootstrap_peers = vec!["127.0.0.1:1".to_string()];
    membership::adopt_from_peers(&lonely)
        .await
        .expect("unreachable peer is ignored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_is_refused_for_the_wrong_cluster_and_can_add_devices_later() {
    let a = first_node(1, 1, 0).await;
    let (_listener, addr) = reserve_port().await;
    let (config, _dirs, _state) = make_config(1, addr, vec![]);
    match membership::join(&config, a.addr, Uuid::new_v4(), false).await {
        Err(membership::MembershipError::PeerUnreachable { .. })
        | Err(membership::MembershipError::WrongCluster { .. }) => {}
        other => panic!("expected refusal, got {other:?}"),
    }
    // Devices were not initialised for a refused join.
    assert!(!config.devices[0]
        .join("DISTRIBUTED-JBOD-DEVICE.json")
        .exists());

    // Add a device to a: list it in the config, add it, reopen.
    let extra = tempfile::tempdir().expect("temp dir");
    let mut config = a.config.clone();
    config.devices.push(extra.path().to_path_buf());
    let document = membership::add_devices(
        &config,
        &[extra.path().to_path_buf()],
        a.addr,
        a.node.cluster_id(),
        false,
    )
    .await
    .expect("add device");
    assert_eq!(document.version, 2);
    assert_eq!(document.devices.len(), 2);
    assert_eq!(a.node.document_version(), 2, "the running node adopted it");
    // Adding it again is refused.
    assert!(matches!(
        membership::add_devices(
            &config,
            &[extra.path().to_path_buf()],
            a.addr,
            a.node.cluster_id(),
            false
        )
        .await,
        Err(membership::MembershipError::AlreadyMember { .. })
    ));
    let reopened = Node::open(config).expect("reopen with the new device");
    assert_eq!(reopened.devices().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_that_differ_at_the_same_version_stop_proposals_and_sync() {
    let a = first_node(1, 1, 1).await;
    let b = joined_node(1, &a).await;
    // Corrupt the invariant by hand: give b a different document with
    // the same version number.
    let mut forged = b.node.document();
    forged.version += 1;
    forged.headroom = 0.45;
    b.node
        .apply_document(forged.clone())
        .expect("apply forged to b");
    let mut other = a.node.document();
    other.version += 1;
    other.headroom = 0.05;
    a.node
        .apply_document(other.clone())
        .expect("apply other to a");
    assert_eq!(a.node.document_version(), b.node.document_version());
    assert_ne!(a.node.document(), b.node.document());

    let mut next = other.clone();
    next.version += 1;
    assert!(matches!(
        membership::propose(&Connector::plain(), &other, &next).await,
        Err(membership::MembershipError::Diverged { .. })
    ));
    assert!(matches!(
        membership::sync(&Connector::plain(), a.addr, a.node.cluster_id()).await,
        Err(membership::MembershipError::Diverged { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_retried_after_a_partial_apply_completes_without_a_new_version() {
    let a = first_node(1, 1, 1).await;
    let (_listener, addr) = reserve_port().await;
    let (config, _dirs, _state) = make_config(1, addr, vec![]);
    let first = membership::join(&config, a.addr, a.node.cluster_id(), false)
        .await
        .expect("join");
    assert_eq!(first.version, 2);
    // Joining again with the same configuration proposes nothing.
    let again = membership::join(&config, a.addr, a.node.cluster_id(), false)
        .await
        .expect("join again");
    assert_eq!(again.version, 2);
    assert_eq!(a.node.document_version(), 2);
}

/// Run a cluster scrub through `conn`, collecting events and the end.
async fn run_scrub(
    conn: &mut Connection,
    repair: bool,
) -> (
    Vec<djbod_proto::message::ScrubEvent>,
    djbod_proto::message::StreamEnd,
) {
    let id = conn.start_scrub(None, repair).await.expect("start scrub");
    let mut events = Vec::new();
    loop {
        match conn.next_scrub_event(id).await.expect("scrub event") {
            Ok(event) => events.push(event),
            Err(end) => return (events, end),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_scrub_finds_local_and_cross_node_damage_and_repairs_it() {
    use djbod_core::scrub::Finding;
    use djbod_proto::message::{ClusterFinding, ScrubEvent};
    let a = first_node(2, 3, 1).await;
    let b = joined_node(2, &a).await;
    let c = joined_node(2, &b).await;
    let nodes = [&a, &b, &c];
    let mut client = a.client().await;

    let mut records = Vec::new();
    for i in 0..3u8 {
        let body = xorshift64_bytes(2 * 3 * BLOCK as usize + i as usize, i as u64 + 10);
        let key = format!("obj-{i}");
        client
            .put_object(&key, &body, 100_000, None)
            .await
            .expect("put");
        match client
            .request(Request::HeadObject { key: key.clone() })
            .await
            .expect("head")
        {
            Response::HeadObject { record } => records.push(record),
            other => panic!("{other:?}"),
        }
    }

    // Clean cluster: summaries from six devices, no findings, clean end.
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ScrubEvent::NodeSummary { .. }))
            .count(),
        6
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        ScrubEvent::NodeFinding { .. } | ScrubEvent::ClusterFinding(_)
    )));

    // Locate a shard file on a given node for a record.
    let shard_path = |record: &djbod_core::record::MetadataRecord, shard_index: u8| {
        let device_id = record
            .device_for(djbod_core::erasure::ShardIndex(shard_index))
            .expect("device");
        for n in nodes {
            if let Some(device) = n.node.device(device_id) {
                return (
                    n.node.id(),
                    device_id,
                    device.object_directory(&record.key_hash).join(
                        djbod_core::layout::shard_file_name(
                            &record.version,
                            djbod_core::erasure::ShardIndex(shard_index),
                        ),
                    ),
                );
            }
        }
        panic!("no node holds device {device_id:?}");
    };

    // Damage 1: a corrupt block in obj-0's shard 1 (local finding).
    let (node1, device1, path1) = shard_path(&records[0], 1);
    let mut bytes = std::fs::read(&path1).expect("read");
    bytes[4096 + BLOCK as usize + 3] ^= 0x01;
    std::fs::write(&path1, &bytes).expect("write");

    // Damage 2: obj-1's shard 2 file deleted (local finding: record
    // without shard; cross-node: shard missing on holder).
    let (_node2, device2, path2) = shard_path(&records[1], 2);
    std::fs::remove_file(&path2).expect("remove");

    // Damage 3: obj-2 loses both record and shard on one device, which
    // only the cross-node check can see.
    let (_node3, device3, path3) = shard_path(&records[2], 3);
    std::fs::remove_file(&path3).expect("remove shard");
    std::fs::remove_file(
        path3
            .parent()
            .unwrap()
            .join(djbod_core::layout::record_file_name(&records[2].version)),
    )
    .expect("remove record");

    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    let local: Vec<(&djbod_core::cluster::NodeId, &DeviceId, &Finding)> = events
        .iter()
        .filter_map(|e| match e {
            ScrubEvent::NodeFinding {
                node,
                device,
                finding,
            } => Some((node, device, finding)),
            _ => None,
        })
        .collect();
    assert!(
        local.iter().any(|(n, d, f)| **n == node1
            && **d == device1
            && matches!(f, Finding::ShardBlocksCorrupt { stripes, .. } if stripes == &vec![1])),
        "{local:?}"
    );
    assert!(
        local.iter().any(|(_, d, f)| **d == device2
            && matches!(f, Finding::RecordWithoutShard { shard_index: 2, .. })),
        "{local:?}"
    );
    // Device 3 has nothing left for obj-2, so no local finding about obj-2
    // there (it may hold damaged shards of the other objects).
    assert!(!local
        .iter()
        .any(|(_, d, f)| **d == device3 && f.repair_key() == Some("obj-2")));
    let cluster: Vec<&ClusterFinding> = events
        .iter()
        .filter_map(|e| match e {
            ScrubEvent::ClusterFinding(f) => Some(f),
            _ => None,
        })
        .collect();
    assert!(
        cluster.iter().any(|f| matches!(f, ClusterFinding::ShardMissingOnHolder { key, device, shard_index: 2, .. } if key == "obj-1" && *device == device2)),
        "{cluster:?}"
    );
    assert!(
        cluster.iter().any(|f| matches!(f, ClusterFinding::RecordsInconsistent { key, detail, .. } if key == "obj-2" && detail.contains("3 record copies found, 4 expected"))),
        "{cluster:?}"
    );

    // With repair: all three are rebuilt. obj-2's missing record copy is
    // rewritten from the three agreeing copies (18.4.2) along with its
    // shard.
    let (events, end) = run_scrub(&mut client, true).await;
    assert!(end.error.is_none(), "{end:?}");
    let repaired: Vec<(&str, &djbod_proto::message::RepairReport)> = events
        .iter()
        .filter_map(|e| match e {
            ScrubEvent::Repaired { key, report } => Some((key.as_str(), report)),
            _ => None,
        })
        .collect();
    let keys: Vec<&str> = repaired.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, vec!["obj-0", "obj-1", "obj-2"]);
    let obj2 = repaired
        .iter()
        .find(|(k, _)| *k == "obj-2")
        .expect("obj-2")
        .1;
    assert_eq!(obj2.record_copies_rewritten, vec![device3]);
    assert_eq!(obj2.shards.iter().filter(|s| s.rewritten).count(), 1);
    assert!(!events
        .iter()
        .any(|e| matches!(e, ScrubEvent::RepairFailed { .. })));
    for key in ["obj-0", "obj-1", "obj-2"] {
        let mut c = c.client().await;
        c.get_object(key).await.expect("reads after repair");
    }

    // A second scrub is clean.
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ScrubEvent::NodeFinding { .. } | ScrubEvent::ClusterFinding(_)
        )),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_scrub_reports_a_node_it_cannot_reach_and_still_scrubs_the_rest() {
    use djbod_proto::message::ScrubEvent;
    let a = first_node(1, 1, 1).await;
    let mut b = joined_node(1, &a).await;
    let mut client = a.client().await;
    client
        .put_object("k", &xorshift64_bytes(BLOCK as usize, 3), 100_000, None)
        .await
        .expect("put");
    b.stop();
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, ScrubEvent::NodeFailed { node, .. } if *node == b.node.id())));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ScrubEvent::NodeSummary { .. }))
            .count(),
        1
    );
    let error = end.error.expect("incomplete scrub is reported");
    assert_eq!(error.code, ErrorCode::NodeUnreachable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_concurrent_write_of_the_same_shard_is_refused() {
    use djbod_core::keyhash::hash_key;
    use djbod_core::version::VersionId;
    let a = first_node(1, 1, 0).await;
    let device = a.node.devices()[0].id();
    let request = Request::PutShard {
        device,
        key_hash: hash_key(b"k"),
        version: VersionId([5u8; 16]),
        shard_index: 0,
        k: 1,
        m: 0,
        block_length: BLOCK,
        object_size: BLOCK,
    };
    let hello = || Hello {
        protocol_version: PROTOCOL_VERSION,
        kind: PeerKind::Node,
        node_id: Some(djbod_core::cluster::NodeId(Uuid::new_v4())),
        cluster_id: a.node.cluster_id(),
        document_version: a.node.document_version(),
        build: None,
    };
    let mut first = Connection::connect(a.addr, hello()).await.expect("connect");
    let id1 = first.send_request(request.clone()).await.expect("send");
    assert_eq!(
        first.read_response(id1).await.expect("ready"),
        Response::PutShardReady
    );

    // While the first write is open, a second request for the same shard
    // on the same device is refused (SPEC 20.1.2.1).
    let mut second = Connection::connect(a.addr, hello()).await.expect("connect");
    match second.request(request.clone()).await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::WriteFailed);
            assert!(
                detail.message.contains("already being written"),
                "{}",
                detail.message
            );
            assert_eq!(detail.device, Some(device));
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    // The first writer abandons; the slot frees; a fresh write is accepted.
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut third = Connection::connect(a.addr, hello()).await.expect("connect");
    let id3 = third.send_request(request).await.expect("send");
    assert_eq!(
        third.read_response(id3).await.expect("ready"),
        Response::PutShardReady
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_shard_across_nodes_and_a_stale_copy_is_found_and_removed_by_scrub() {
    use djbod_proto::message::{ClusterFinding, ScrubEvent};
    let a = first_node(1, 2, 1).await;
    let b = joined_node(1, &a).await;
    let c = joined_node(1, &b).await;
    let mut d = joined_node(1, &c).await;
    let mut client = a.client().await;
    let body = xorshift64_bytes(2 * 2 * BLOCK as usize + 100, 77);
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");
    let before = match client
        .request(Request::HeadObject {
            key: "k".to_string(),
        })
        .await
        .expect("head")
    {
        Response::HeadObject { record } => record,
        other => panic!("{other:?}"),
    };
    let nodes = [&a, &b, &c, &d];
    let owner_of = |device: DeviceId| {
        nodes
            .iter()
            .position(|n| n.node.device(device).is_some())
            .expect("some node holds the device")
    };
    let spare = nodes
        .iter()
        .map(|n| n.node.devices()[0].id())
        .find(|dev| before.shard_on(*dev).is_none())
        .expect("one device holds nothing");

    // Move shard 0 from its holder to the spare device on another node.
    let source = before.shards[0].device;
    let after = match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 0,
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
            assert!(source_cleaned);
            assert!(!rebuilt);
            record
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(after.revision, 1);
    assert_eq!(
        after.device_for(djbod_core::erasure::ShardIndex(0)),
        Some(spare)
    );
    assert_ne!(owner_of(source), owner_of(spare));
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);

    // While a holder's node is down, the lookup itself is fail-stop (16.1).
    let victim = after
        .shards
        .iter()
        .find(|s| owner_of(s.device) == 3)
        .cloned();
    let victim_device_dir = d.node.devices()[0].object_directory(&after.key_hash);
    d.stop();
    match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: 1,
            target: Some(source),
        })
        .await
    {
        Err(ClientError::Remote(detail)) => assert_eq!(detail.code, ErrorCode::NodeUnreachable),
        other => panic!("expected NodeUnreachable, got {other:?}"),
    }
    let listener = TcpListener::bind(d.addr).await.expect("rebind");
    let node = Arc::new(Node::open(d.config.clone()).expect("reopen"));
    d.server = Some(tokio::spawn(server::serve(node.clone(), listener)));
    let mut client = a.client().await;

    // If node d holds a shard, delete its file and move that shard onto
    // the freed source device: the copy fails, the move rebuilds from the
    // other two shards, and the record reaches revision 2. Then restore
    // the old files by hand to make a stale copy for the scrub to find.
    let Some(victim) = victim else {
        return;
    };
    let victim_index = djbod_core::erasure::ShardIndex(victim.index);
    let victim_shard = victim_device_dir.join(djbod_core::layout::shard_file_name(
        &after.version,
        victim_index,
    ));
    let victim_record =
        victim_device_dir.join(djbod_core::layout::record_file_name(&after.version));
    let saved_shard = std::fs::read(&victim_shard).expect("read");
    let saved_record = std::fs::read(&victim_record).expect("read");
    std::fs::remove_file(&victim_shard).expect("remove");
    let moved = match client
        .request(Request::MoveShard {
            key: "k".to_string(),
            shard_index: victim.index,
            target: Some(source),
        })
        .await
        .expect("move")
    {
        Response::MoveShard {
            record,
            source: reported,
            source_cleaned,
            rebuilt,
        } => {
            assert_eq!(reported, victim.device);
            assert!(rebuilt);
            assert!(source_cleaned);
            record
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(moved.revision, 2);
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);

    // The clean-up removed the emptied key directory along with the files.
    std::fs::create_dir_all(&victim_device_dir).expect("mkdir");
    std::fs::write(&victim_shard, &saved_shard).expect("write");
    std::fs::write(&victim_record, &saved_record).expect("write");
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScrubEvent::ClusterFinding(ClusterFinding::StaleCopy {
                device,
                revision: 1,
                current_revision: 2,
                ..
            }) if *device == victim.device
        )),
        "{events:?}"
    );
    let (events, end) = run_scrub(&mut client, true).await;
    assert!(end.error.is_none(), "{end:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScrubEvent::Repaired { report, .. } if report.stale_copies_removed == vec![victim.device]
        )),
        "{events:?}"
    );
    assert!(!victim_shard.exists());
    assert!(!victim_record.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_state_reaches_every_node_and_drain_moves_shards_across_nodes() {
    let a = first_node(1, 2, 1).await;
    let b = joined_node(2, &a).await;
    let c = joined_node(1, &b).await;
    let nodes = [&a, &b, &c];
    let mut client = a.client().await;
    let mut records = Vec::new();
    for i in 0..3u64 {
        let body = xorshift64_bytes(2 * 2 * BLOCK as usize + i as usize, 90 + i);
        let key = format!("obj-{i}");
        client
            .put_object(&key, &body, 100_000, None)
            .await
            .expect("put");
        match client
            .request(Request::HeadObject { key })
            .await
            .expect("head")
        {
            Response::HeadObject { record } => records.push(record),
            other => panic!("{other:?}"),
        }
    }
    // Drain one of b's devices, proposed through c and run through a.
    let device = b.node.devices()[0].id();
    let versions_on_device = records
        .iter()
        .filter(|r| r.shard_on(device).is_some())
        .count();
    let (document, changed) = membership::set_device_state(
        &Connector::plain(),
        c.addr,
        c.node.cluster_id(),
        device,
        DeviceState::Draining,
    )
    .await
    .expect("set state");
    assert!(changed);
    for n in nodes {
        assert_eq!(n.node.document().version, document.version);
        assert_eq!(
            n.node.document().device(device).expect("device").state,
            DeviceState::Draining
        );
    }

    let id = client
        .start_drain(device, false)
        .await
        .expect("start drain");
    let mut moved = 0usize;
    let end = loop {
        match client.next_drain_event(id).await.expect("event") {
            Ok(DrainEvent::Moved { destination, .. }) => {
                moved += 1;
                assert_ne!(destination, device);
            }
            Ok(DrainEvent::Skipped { key, detail, .. }) => panic!("{key} skipped: {detail:?}"),
            Ok(DrainEvent::Deleted { key, .. }) => panic!("{key} reported deleted"),
            Ok(DrainEvent::Estimate { .. }) => {}
            Err(end) => break end,
        }
    };
    assert!(end.error.is_none(), "{end:?}");
    assert_eq!(moved, versions_on_device);
    for i in 0..3 {
        let key = format!("obj-{i}");
        match client
            .request(Request::HeadObject { key: key.clone() })
            .await
            .expect("head")
        {
            Response::HeadObject { record } => assert!(record.shard_on(device).is_none()),
            other => panic!("{other:?}"),
        }
        let mut c_client = c.client().await;
        c_client
            .get_object(&key)
            .await
            .expect("reads from any node");
    }
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            djbod_proto::message::ScrubEvent::NodeFinding { .. }
                | djbod_proto::message::ScrubEvent::ClusterFinding(_)
        )),
        "{events:?}"
    );
}

/// Drain `device` through `client` until the pass ends cleanly.
async fn drain_clean(client: &mut Connection, device: DeviceId) {
    let id = client
        .start_drain(device, false)
        .await
        .expect("start drain");
    loop {
        match client.next_drain_event(id).await.expect("event") {
            Ok(_) => {}
            Err(end) => {
                assert!(end.error.is_none(), "{end:?}");
                return;
            }
        }
    }
}

async fn put_objects(
    client: &mut Connection,
    count: u64,
    seed: u64,
) -> Vec<djbod_core::record::MetadataRecord> {
    let mut records = Vec::new();
    for i in 0..count {
        let body = xorshift64_bytes(2 * 2 * BLOCK as usize + i as usize, seed + i);
        let key = format!("obj-{i}");
        client
            .put_object(&key, &body, 100_000, None)
            .await
            .expect("put");
        match client
            .request(Request::HeadObject { key })
            .await
            .expect("head")
        {
            Response::HeadObject { record } => records.push(record),
            other => panic!("{other:?}"),
        }
    }
    records
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_device_is_refused_while_referenced_and_marks_it_removed_after_a_drain() {
    let a = first_node(1, 2, 1).await;
    let b = joined_node(2, &a).await;
    let c = joined_node(1, &b).await;
    let mut client = a.client().await;
    let records = put_objects(&mut client, 3, 100).await;
    // Four devices, three shards per version: at least one of b's two
    // devices holds a shard of obj-0. Remove that one.
    let device = b
        .node
        .devices()
        .iter()
        .map(|d| d.id())
        .find(|d| records[0].shard_on(*d).is_some())
        .expect("b holds a shard of obj-0");
    let cluster = a.node.cluster_id();

    // An active device cannot be removed, referenced or not (issue #28).
    match membership::remove_device(&Connector::plain(), c.addr, cluster, device).await {
        Err(membership::MembershipError::DeviceActive(found)) => assert_eq!(found, device),
        other => panic!("expected DeviceActive, got {other:?}"),
    }
    assert_eq!(
        a.node.document().device(device).expect("device").state,
        DeviceState::Active
    );
    membership::set_device_state(
        &Connector::plain(),
        c.addr,
        cluster,
        device,
        DeviceState::Draining,
    )
    .await
    .expect("set state");
    // Draining but still holding shards: refused with the keys.
    match membership::remove_device(&Connector::plain(), c.addr, cluster, device).await {
        Err(membership::MembershipError::StillReferenced {
            versions, examples, ..
        }) => {
            assert!(versions >= 1);
            assert!(examples.contains(&"obj-0".to_string()), "{examples:?}");
        }
        other => panic!("expected StillReferenced, got {other:?}"),
    }
    drain_clean(&mut client, device).await;
    let (document, changed) =
        membership::remove_device(&Connector::plain(), c.addr, cluster, device)
            .await
            .expect("remove device");
    assert!(changed);
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
        assert_eq!(
            n.node
                .document()
                .device(device)
                .expect("still listed")
                .state,
            DeviceState::Removed
        );
    }
    let (_, changed) = membership::remove_device(&Connector::plain(), c.addr, cluster, device)
        .await
        .expect("remove again");
    assert!(!changed);
    // Three active devices remain, exactly k+m: writes and reads go on.
    let body = xorshift64_bytes(BLOCK as usize, 7);
    client
        .put_object("after", &body, 100_000, None)
        .await
        .expect("put");
    for i in 0..3 {
        client.get_object(&format!("obj-{i}")).await.expect("get");
    }
    match client.request(Request::Status).await.expect("status") {
        Response::Status { devices, .. } => {
            let removed = devices.iter().find(|d| d.device == device).expect("listed");
            assert_eq!(removed.state, DeviceState::Removed);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_node_drops_it_after_a_drain_and_the_node_stops_and_can_rejoin_only_wiped() {
    let a = first_node(1, 2, 1).await;
    let b = joined_node(1, &a).await;
    let c = joined_node(1, &b).await;
    let mut d = joined_node(1, &c).await;
    let cluster = a.node.cluster_id();
    let mut client = a.client().await;
    let records = put_objects(&mut client, 3, 200).await;
    let device = d.node.devices()[0].id();
    let d_id = d.node.id();
    let old_device_path = d.config.devices[0].clone();

    // A node with an active device cannot be removed (issue #28).
    match membership::remove_node(&Connector::plain(), a.addr, cluster, d_id).await {
        Err(membership::MembershipError::NodeHasActiveDevices { node, devices }) => {
            assert_eq!(node, d_id);
            assert_eq!(devices, vec![device]);
        }
        other => panic!("expected NodeHasActiveDevices, got {other:?}"),
    }
    membership::set_device_state(
        &Connector::plain(),
        a.addr,
        cluster,
        device,
        DeviceState::Draining,
    )
    .await
    .expect("set state");
    if records.iter().any(|r| r.shard_on(device).is_some()) {
        match membership::remove_node(&Connector::plain(), a.addr, cluster, d_id).await {
            Err(membership::MembershipError::StillReferenced { .. }) => {}
            other => panic!("expected StillReferenced, got {other:?}"),
        }
        drain_clean(&mut client, device).await;
    }
    let document = membership::remove_node(&Connector::plain(), a.addr, cluster, d_id)
        .await
        .expect("remove node");
    assert!(document.node(d_id).is_none());
    assert!(document.device(device).is_none());
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
    }
    // d acknowledged the document that drops it, and stopped serving.
    let server = d.server.take().expect("server handle");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("serve returned after removal")
        .expect("serve task");
    assert!(d.node.is_removed());
    assert!(
        Connection::connect(d.addr, Connection::client_hello(cluster))
            .await
            .is_err()
    );
    // It cannot come back as it was.
    match Node::open(d.config.clone()) {
        Err(djbod_node::node::NodeError::NotAMember { node }) => assert_eq!(node, d_id),
        Err(other) => panic!("expected NotAMember, got {other:?}"),
        Ok(_) => panic!("expected NotAMember, but the node opened"),
    }
    // Nor can its device rejoin unwiped; wiped, it joins as a new device.
    match membership::join(&d.config, a.addr, cluster, false).await {
        Err(membership::MembershipError::RemovedDevice {
            path,
            device: found,
        }) => {
            assert_eq!(path, old_device_path);
            assert_eq!(found, device);
        }
        other => panic!("expected RemovedDevice, got {other:?}"),
    }
    let document = membership::join(&d.config, a.addr, cluster, true)
        .await
        .expect("join wiped");
    let new_device = document
        .devices
        .iter()
        .find(|e| e.node == d_id)
        .expect("d has a device again")
        .id;
    assert_ne!(new_device, device);
    let reopened = Arc::new(Node::open(d.config.clone()).expect("reopen"));
    assert!(reopened.devices()[0]
        .key_directories()
        .expect("list")
        .is_empty());
    let listener = TcpListener::bind(d.addr).await.expect("rebind");
    tokio::spawn(server::serve(reopened.clone(), listener));
    let body = xorshift64_bytes(BLOCK as usize, 9);
    client
        .put_object("after", &body, 100_000, None)
        .await
        .expect("put with d back");
    for i in 0..3 {
        client.get_object(&format!("obj-{i}")).await.expect("get");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_node_is_removed_by_force_and_its_shards_are_rebuilt_elsewhere() {
    let a = first_node(1, 2, 1).await;
    let b = joined_node(1, &a).await;
    let c = joined_node(1, &b).await;
    let mut d = joined_node(1, &c).await;
    let cluster = a.node.cluster_id();
    let mut client = a.client().await;
    let records = put_objects(&mut client, 3, 300).await;
    let device = d.node.devices()[0].id();
    let d_id = d.node.id();
    let affected: Vec<String> = records
        .iter()
        .filter(|r| r.shard_on(device).is_some())
        .map(|r| r.key.clone())
        .collect();

    // A live node cannot be forced.
    match membership::plan_forced_removal(&Connector::plain(), a.addr, cluster, d_id).await {
        Err(membership::MembershipError::NodeIsAlive { node, .. }) => assert_eq!(node, d_id),
        other => panic!("expected NodeIsAlive, got {other:?}"),
    }

    d.stop();
    drop(d);
    let plan = membership::plan_forced_removal(&Connector::plain(), a.addr, cluster, d_id)
        .await
        .expect("plan");
    assert_eq!(plan.devices, vec![device]);
    let mut planned: Vec<String> = plan.affected.iter().map(|r| r.key.clone()).collect();
    planned.sort();
    assert_eq!(planned, affected);
    assert!(plan.unrecoverable().is_empty(), "m = 1 and one device");
    let document = membership::execute_forced_removal(&Connector::plain(), &plan)
        .await
        .expect("execute");
    assert!(document.node(d_id).is_none());
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
    }

    // Step 3: one repair per affected key relocates the lost shards.
    let mut client = b.client().await;
    for key in &affected {
        let report = match client
            .request(Request::RepairObject { key: key.clone() })
            .await
            .expect("repair")
        {
            Response::RepairObject(report) => report,
            other => panic!("{other:?}"),
        };
        let lost: Vec<_> = report
            .shards
            .iter()
            .filter(|s| s.condition == ShardCondition::Lost)
            .collect();
        assert_eq!(lost.len(), 1, "{report:?}");
        assert_eq!(lost[0].device, device);
        assert!(lost[0].relocated_to.is_some());
    }
    for (i, record) in records.iter().enumerate() {
        let key = format!("obj-{i}");
        let (_, got) = client.get_object(&key).await.expect("get");
        assert_eq!(got.len(), record.size as usize);
    }
    let (events, end) = run_scrub(&mut client, false).await;
    assert!(end.error.is_none(), "{end:?}");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            djbod_proto::message::ScrubEvent::NodeFinding { .. }
                | djbod_proto::message::ScrubEvent::ClusterFinding(_)
        )),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paged_listing_over_several_nodes_yields_every_key_once() {
    use djbod_proto::message::ListQuery;
    let a = first_node(1, 2, 1).await;
    let b = joined_node(1, &a).await;
    let c = joined_node(1, &b).await;
    let d = joined_node(1, &c).await;
    let mut client = a.client().await;
    // Seven objects at 2+1 over four devices: every key's record is on
    // three of the four nodes, so every node's page overlaps the others'.
    let records = put_objects(&mut client, 7, 400).await;
    let mut expected: Vec<String> = records.iter().map(|r| r.key.clone()).collect();
    expected.sort();
    for limit in [1u32, 2, 3, 100] {
        let mut walked: Vec<String> = Vec::new();
        let mut start_after: Option<String> = None;
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
                    assert!(!keys.is_empty());
                    assert!(keys.len() <= limit as usize);
                    start_after = keys.last().map(|k| k.key.clone());
                    walked.extend(keys.into_iter().map(|k| k.key));
                    if !truncated {
                        break;
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(walked, expected, "limit {limit}");
    }
    // The other node is as good a coordinator.
    let mut client = d.client().await;
    match client
        .request(Request::ListKeys(ListQuery {
            prefix: Some("obj-".to_string()),
            start_after: Some("obj-4".to_string()),
            limit: None,
        }))
        .await
        .expect("list")
    {
        Response::ListKeys { keys, truncated } => {
            let got: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(got, vec!["obj-5", "obj-6"]);
            assert!(!truncated);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_key_on_one_node_does_not_hide_a_long_key_left_out_by_another() {
    use djbod_proto::message::{ListQuery, MAX_LIST_PAGE_BYTES};
    // Two nodes, one device each, no parity, so every key lives on exactly
    // one node and placement can be steered with the device state.
    let a = first_node(1, 1, 0).await;
    let b = joined_node(1, &a).await;
    let cluster = a.node.cluster_id();
    let device_a = a.node.devices()[0].id();
    let device_b = b.node.devices()[0].id();
    let mut client = a.client().await;
    let mut next = a.node.document();
    next.version += 1;
    next.max_key_bytes = djbod_core::cluster::LIMIT_MAX_KEY_BYTES;
    membership::propose(&Connector::plain(), &a.node.document(), &next)
        .await
        .expect("raise the key limit");

    // Node a holds nine long keys a0..a8, each a little under 1 MiB, so
    // its page holds eight of them and leaves a8 out for want of room.
    // Node b holds one short key "b", which sorts after all of them.
    let long = MAX_LIST_PAGE_BYTES / 8 - 100;
    membership::set_device_state(
        &Connector::plain(),
        a.addr,
        cluster,
        device_b,
        DeviceState::Draining,
    )
    .await
    .expect("set state");
    let mut expected: Vec<String> = Vec::new();
    for i in 0..9 {
        let key = format!("a{i}-") + &"k".repeat(long - 3);
        client
            .put_object(&key, b"x", 100_000, None)
            .await
            .expect("put");
        expected.push(key);
    }
    membership::set_device_state(
        &Connector::plain(),
        a.addr,
        cluster,
        device_b,
        DeviceState::Active,
    )
    .await
    .expect("set state");
    membership::set_device_state(
        &Connector::plain(),
        a.addr,
        cluster,
        device_a,
        DeviceState::Draining,
    )
    .await
    .expect("set state");
    client
        .put_object("b", b"x", 100_000, None)
        .await
        .expect("put");
    expected.push("b".to_string());
    membership::set_device_state(
        &Connector::plain(),
        a.addr,
        cluster,
        device_a,
        DeviceState::Active,
    )
    .await
    .expect("set state");

    // The first page must stop at a7, not run on to "b" past the unseen
    // a8; the walk must then yield all ten keys.
    let mut walked: Vec<String> = Vec::new();
    let mut start_after: Option<String> = None;
    let mut first = true;
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
                if first {
                    assert_eq!(keys.len(), 8, "first page stops at the horizon");
                    assert!(keys.last().expect("non-empty").key.starts_with("a7-"));
                    assert!(truncated);
                    first = false;
                }
                start_after = keys.last().map(|k| k.key.clone());
                walked.extend(keys.into_iter().map(|k| k.key));
                if !truncated {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(walked, expected);
}

/// The document as saved in a node's state directory.
fn saved_document(config: &NodeConfig) -> ClusterDocument {
    serde_json::from_str(&std::fs::read_to_string(Node::document_path_for(config)).expect("read"))
        .expect("parse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nodes_address_can_be_changed_through_another_node() {
    let a = first_node(1, 1, 1).await;
    let mut b = joined_node(1, &a).await;
    let cluster = a.node.cluster_id();
    let body = xorshift64_bytes(BLOCK as usize, 9);
    let mut client = a.client().await;
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");

    // b also listens on a new port; the change then points everyone there.
    let (new_listener, new_addr) = reserve_port().await;
    let new_server = tokio::spawn(server::serve(b.node.clone(), new_listener));
    let (document, changed) = membership::set_node_addresses(
        &Connector::plain(),
        a.addr,
        cluster,
        b.node.id(),
        vec![new_addr.to_string()],
    )
    .await
    .expect("set address");
    assert!(changed);
    assert_eq!(
        document.node(b.node.id()).unwrap().addresses,
        vec![new_addr.to_string()]
    );
    assert_eq!(a.node.document_version(), document.version);
    assert_eq!(b.node.document_version(), document.version);
    assert_eq!(
        saved_document(&b.config)
            .node(b.node.id())
            .unwrap()
            .addresses,
        vec![new_addr.to_string()],
        "b's own saved document has the new address"
    );
    let fetched = membership::fetch_document(&Connector::plain(), a.addr, cluster)
        .await
        .expect("fetch");
    assert_eq!(
        fetched.node(b.node.id()).unwrap().addresses,
        vec![new_addr.to_string()]
    );

    // With the old listener gone, a reaches b only at the new address.
    b.stop();
    let mut client = a.client().await;
    let (_, got) = client.get_object("k").await.expect("get via new address");
    assert_eq!(got, body);
    client
        .put_object("k2", &body, 100_000, None)
        .await
        .expect("put via new address");

    // Setting the same list again changes nothing.
    let (_, changed) = membership::set_node_addresses(
        &Connector::plain(),
        a.addr,
        cluster,
        b.node.id(),
        vec![new_addr.to_string()],
    )
    .await
    .expect("same address");
    assert!(!changed);

    // Refusals: not an address, an empty list, another node's address.
    for bad in [
        vec!["nowhere".to_string()],
        vec![],
        vec![a.addr.to_string()],
    ] {
        let result = membership::set_node_addresses(
            &Connector::plain(),
            a.addr,
            cluster,
            b.node.id(),
            bad.clone(),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(membership::MembershipError::Node(
                    djbod_node::node::NodeError::InvalidDocument(_)
                ))
            ),
            "{bad:?}: {result:?}"
        );
    }
    assert_eq!(
        a.node.document_version(),
        document.version,
        "refusals change nothing"
    );
    new_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarting_node_adopts_its_configured_address() {
    let mut a = first_node(1, 1, 1).await;
    let mut b = joined_node(1, &a).await;
    let body = xorshift64_bytes(BLOCK as usize, 10);
    let mut client = a.client().await;
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");
    let before = a.node.document_version();

    // b comes back on another port: startup adoption tells the cluster.
    b.stop();
    let (listener, new_addr) = reserve_port().await;
    let mut config = b.config.clone();
    config.listen = new_addr;
    let adopted = membership::adopt_from_peers(&config).await.expect("adopt");
    assert_eq!(adopted.version, before + 1);
    assert_eq!(
        adopted.node(b.node.id()).unwrap().addresses,
        vec![new_addr.to_string()]
    );
    assert_eq!(a.node.document_version(), before + 1, "a holds the change");
    assert_eq!(saved_document(&config).version, before + 1);
    // Nothing more to do on a second start.
    let again = membership::adopt_from_peers(&config)
        .await
        .expect("adopt again");
    assert_eq!(again.version, before + 1);

    let node = Arc::new(Node::open(config.clone()).expect("reopen"));
    b.server = Some(tokio::spawn(server::serve(node.clone(), listener)));
    let mut client = a.client().await;
    let (_, got) = client.get_object("k").await.expect("get via new address");
    assert_eq!(got, body);

    // Moving again while a is down is refused: nobody could be told.
    b.stop();
    a.stop();
    let (_listener, third) = reserve_port().await;
    config.listen = third;
    let result = membership::adopt_from_peers(&config).await;
    assert!(
        matches!(
            result,
            Err(membership::MembershipError::AddressChangeFailed { .. })
        ),
        "{result:?}"
    );
    assert_eq!(
        saved_document(&config).version,
        before + 1,
        "nothing was saved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lone_node_adopts_its_configured_address_by_itself() {
    let mut a = first_node(1, 1, 0).await;
    let body = xorshift64_bytes(BLOCK as usize, 11);
    let mut client = a.client().await;
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");
    a.stop();

    let (listener, new_addr) = reserve_port().await;
    let mut config = a.config.clone();
    config.listen = new_addr;
    let adopted = membership::adopt_from_peers(&config).await.expect("adopt");
    assert_eq!(adopted.version, 2);
    assert_eq!(adopted.nodes[0].addresses, vec![new_addr.to_string()]);
    let node = Arc::new(Node::open(config.clone()).expect("reopen"));
    a.server = Some(tokio::spawn(server::serve(node.clone(), listener)));
    let hello = Connection::client_hello(node.cluster_id());
    let mut client = Connection::connect(new_addr, hello).await.expect("connect");
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
}

/// SPEC 6.2.6.4: a document carrying a field this build does not know is
/// refused, on the wire with an error naming the field and this node's
/// build, and in this node's own file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_with_a_field_this_build_does_not_know_is_refused() {
    let mut a = first_node(1, 1, 0).await;
    let before = a.node.document_version();
    let mut next = a.node.document();
    next.version += 1;

    // The request as a newer build would send it: the document with one
    // more field. Externally tagged: {"ApplyClusterConfig": {"document": {..}}}.
    let request = Request::ApplyClusterConfig { document: next };
    let mut value = ciborium::Value::serialized(&request).expect("to cbor value");
    let document = value.as_map_mut().expect("request map")[0]
        .1
        .as_map_mut()
        .expect("variant map")[0]
        .1
        .as_map_mut()
        .expect("document map");
    document.push((
        ciborium::Value::Text("colour".to_string()),
        ciborium::Value::Text("blue".to_string()),
    ));
    let payload = djbod_proto::codec::encode_cbor(&value).expect("encode");

    let mut stream = TcpStream::connect(a.addr).await.expect("connect");
    let hello = Message::Hello(Connection::client_hello(a.node.cluster_id()));
    wire::write_message(&mut stream, &hello)
        .await
        .expect("hello");
    match wire::read_message(&mut stream).await.expect("peer hello") {
        Message::Hello(peer) => assert_eq!(peer.build.as_deref(), Some(djbod_node::BUILD)),
        other => panic!("expected Hello, got {other:?}"),
    }
    stream
        .write_all(&Frame::new(MessageType::Request, 7, payload).encode())
        .await
        .expect("send");
    match wire::read_message(&mut stream).await.expect("response") {
        Message::Response {
            id: 7,
            response: Response::Error(detail),
        } => {
            assert_eq!(detail.code, ErrorCode::ProtocolViolation);
            assert!(
                detail.message.contains("unknown field `colour`"),
                "{}",
                detail.message
            );
            assert!(
                detail.message.contains(djbod_node::BUILD),
                "{}",
                detail.message
            );
            assert_eq!(detail.node, Some(a.node.id()));
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(
        matches!(
            wire::read_message(&mut stream).await,
            Err(wire::WireError::Closed)
        ),
        "the connection is closed after an undecodable request"
    );
    assert_eq!(a.node.document_version(), before, "nothing was applied");

    // The same field in this node's own copy, as a newer build would have
    // written it: refused with the same reason instead of read without it.
    a.stop();
    let path = Node::document_path_for(&a.config);
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
    json["nodes"][0]["colour"] = "blue".into();
    std::fs::write(&path, serde_json::to_string(&json).expect("to json")).expect("write");
    match Node::load_document_for(&a.config) {
        Err(NodeError::BadDocument { reason, .. }) => {
            assert!(reason.contains("unknown field `colour`"), "{reason}")
        }
        other => panic!("expected refusal, got {other:?}"),
    }
}
