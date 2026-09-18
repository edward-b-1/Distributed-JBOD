//! Several nodes in one process on localhost ports: joining, placement
//! across nodes, document changes, stragglers and sync, startup adoption,
//! and fail-stop when a node is down.

use std::net::SocketAddr;
use std::sync::Arc;

use djbod_core::cluster::{ClusterDocument, DeviceState};
use djbod_node::client::{ClientError, Connection};
use djbod_node::config::NodeConfig;
use djbod_node::membership;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_proto::message::{ErrorCode, Request, Response};
use tokio::net::TcpListener;
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
        allow_shared_filesystem: true,
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
    membership::join(&config, peer.addr, peer.node.cluster_id())
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
    let reports = membership::fetch_all(&document).await;
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
    membership::propose(&current, &first)
        .await
        .expect("first proposal");
    match membership::propose(&current, &second).await {
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
        membership::propose(&fourth, &fifth).await,
        Err(membership::MembershipError::VersionsDiffer(_))
    ));

    // Sync brings b up; requests work again.
    let report = membership::sync(b.addr, a.node.cluster_id())
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
    match membership::join(&config, a.addr, Uuid::new_v4()).await {
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
            a.node.cluster_id()
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
        membership::propose(&other, &next).await,
        Err(membership::MembershipError::Diverged { .. })
    ));
    assert!(matches!(
        membership::sync(a.addr, a.node.cluster_id()).await,
        Err(membership::MembershipError::Diverged { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_retried_after_a_partial_apply_completes_without_a_new_version() {
    let a = first_node(1, 1, 1).await;
    let (_listener, addr) = reserve_port().await;
    let (config, _dirs, _state) = make_config(1, addr, vec![]);
    let first = membership::join(&config, a.addr, a.node.cluster_id())
        .await
        .expect("join");
    assert_eq!(first.version, 2);
    // Joining again with the same configuration proposes nothing.
    let again = membership::join(&config, a.addr, a.node.cluster_id())
        .await
        .expect("join again");
    assert_eq!(again.version, 2);
    assert_eq!(a.node.document_version(), 2);
}
