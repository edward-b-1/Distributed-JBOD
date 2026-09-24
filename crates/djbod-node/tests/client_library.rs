//! The client library (SPEC 20.8) against nodes in this process: learning
//! the cluster id, every operation, failover to another node, and the
//! blocking facade.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;

use djbod_client::{blocking, Client, ClientError, ClientOptions};
use djbod_node::config::NodeConfig;
use djbod_node::membership;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_proto::message::ListQuery;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

const BLOCK: u64 = 64 * 1024;

struct TestNode {
    node: Arc<Node>,
    addr: SocketAddr,
    _server: JoinHandle<()>,
    _dirs: Vec<tempfile::TempDir>,
    _state: tempfile::TempDir,
}

fn make_config(
    listen: SocketAddr,
    bootstrap: Vec<String>,
) -> (NodeConfig, Vec<tempfile::TempDir>, tempfile::TempDir) {
    let dirs = vec![tempfile::tempdir().expect("temp dir")];
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

/// A TCP forwarder to `target` that the test can sever by aborting the
/// returned task, which drops both sockets: a node whose connection dies.
async fn forwarder(target: SocketAddr) -> (SocketAddr, JoinHandle<()>) {
    let (listener, addr) = reserve_port().await;
    // One connection at a time, forwarded within this task, so that
    // aborting the task severs the connection in flight.
    let task = tokio::spawn(async move {
        loop {
            let (mut inbound, _) = listener.accept().await.expect("accept");
            let mut outbound = tokio::net::TcpStream::connect(target)
                .await
                .expect("connect");
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        }
    });
    (addr, task)
}

/// A `1+1` cluster of two nodes with one device each.
async fn two_nodes() -> (TestNode, TestNode) {
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(addr, vec![]);
    let node = Arc::new(
        Node::init_cluster(
            config,
            ClusterParameters {
                k: 1,
                m: 1,
                block_size: BLOCK,
                headroom: 0.0,
                ..ClusterParameters::default()
            },
        )
        .expect("init cluster"),
    );
    let a = TestNode {
        _server: tokio::spawn(server::serve(node.clone(), listener)),
        node,
        addr,
        _dirs: dirs,
        _state: state,
    };
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(addr, vec![a.addr.to_string()]);
    membership::join(&config, a.addr, a.node.cluster_id(), false)
        .await
        .expect("join");
    let node = Arc::new(Node::open(config).expect("open joined node"));
    let b = TestNode {
        _server: tokio::spawn(server::serve(node.clone(), listener)),
        node,
        addr,
        _dirs: dirs,
        _state: state,
    };
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_learns_the_cluster_id_does_every_operation_and_fails_over() {
    let (a, b) = two_nodes().await;
    let cluster = a.node.cluster_id();
    // A dead address first, then a through a forwarder the test can cut,
    // then b. The dead one is skipped and the id is learned from a.
    let (_unused, dead) = reserve_port().await;
    drop(_unused);
    let (via_a, link_to_a) = forwarder(a.addr).await;
    let mut client = Client::connect(ClientOptions::new(vec![dead, via_a, b.addr]))
        .await
        .expect("connect");
    assert_eq!(client.cluster_id(), cluster);
    assert_eq!(client.node_address(), Some(via_a));

    let body: Vec<u8> = (0..(2 * BLOCK as usize)).map(|i| (i % 251) as u8).collect();
    let version = client
        .put("k", &body, Some("application/octet-stream".to_string()))
        .await
        .expect("put");
    let (read, got) = client.get("k").await.expect("get");
    assert_eq!(got, body);
    assert_eq!(read.record.version, version);
    assert!(read.reconstructed.is_empty());
    assert_eq!(
        read.record.content_type.as_deref(),
        Some("application/octet-stream")
    );
    let head = client.head("k").await.expect("head");
    assert_eq!(head.size, body.len() as u64);
    let missing = client.head("nothing").await.expect_err("missing");
    assert!(missing.is_not_found(), "{missing}");

    // Paging: three keys, one per page, then all at once.
    for key in ["k2", "k3"] {
        client.put(key, b"x", None).await.expect("put");
    }
    let page = client
        .list(ListQuery {
            prefix: None,
            start_after: None,
            limit: Some(1),
        })
        .await
        .expect("list");
    assert_eq!(page.keys.len(), 1);
    assert_eq!(page.next_start_after(), Some("k"));
    let all = client.list_all(None).await.expect("list all");
    assert_eq!(
        all.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
        vec!["k", "k2", "k3"]
    );
    let under_prefix = client.list_all(Some("k2")).await.expect("list prefix");
    assert_eq!(under_prefix.len(), 1);

    let status = client.status().await.expect("status");
    assert_eq!(status.cluster_id, cluster);
    assert_eq!(status.devices.len(), 2);
    let identity = client.identity().await.expect("identity");
    assert_eq!(identity.node, Some(a.node.id()));
    assert_eq!(identity.build, djbod_client::BUILD);
    let document = client.cluster_document().await.expect("document");
    assert_eq!(document.version, a.node.document_version());
    let report = client.repair("k").await.expect("repair");
    assert_eq!(report.key, "k");

    // The connection to a dies: the next request goes to b over a fresh
    // connection, and everything works from there.
    link_to_a.abort();
    let status = client.status().await.expect("status after failover");
    assert_eq!(status.coordinator, b.node.id());
    assert_eq!(client.node_address(), Some(b.addr));
    let (_, got) = client.get("k3").await.expect("get via b");
    assert_eq!(got, b"x");
    client.delete("k2").await.expect("delete via b");
    assert!(client.head("k2").await.expect_err("gone").is_not_found());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_needs_a_reachable_node() {
    assert!(matches!(
        Client::connect(ClientOptions::new(vec![])).await,
        Err(ClientError::NoNodes)
    ));
    let (_unused, dead) = reserve_port().await;
    drop(_unused);
    match Client::connect(ClientOptions::new(vec![dead])).await {
        Err(ClientError::Unreachable(attempts)) => assert_eq!(attempts.len(), 1),
        other => panic!("expected unreachable, got {:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_blocking_client_does_the_same_without_async() {
    let (a, _b) = two_nodes().await;
    let options = ClientOptions::new(vec![a.addr]).cluster(a.node.cluster_id());
    // Its own runtime, so it runs on a plain thread, as a binding would.
    let result = std::thread::spawn(move || -> Result<(usize, Vec<u8>, u64), ClientError> {
        let mut client = blocking::Client::connect(options)?;
        client.put("one", b"first", None)?;
        let body: Vec<u8> = vec![7u8; BLOCK as usize + 5];
        client.put_from_reader(
            "two",
            body.len() as u64,
            Cursor::new(body.clone()),
            None,
            Default::default(),
        )?;
        let mut sink = Vec::new();
        let record = client.get_to_writer("two", &mut sink)?.record;
        assert_eq!(sink, body);
        let keys = client.list_all(None)?;
        let status = client.status()?;
        Ok((
            keys.len(),
            client.get("one")?.1,
            record.size + status.devices.len() as u64,
        ))
    })
    .join()
    .expect("thread")
    .expect("blocking client");
    assert_eq!(result.0, 2);
    assert_eq!(result.1, b"first");
    assert_eq!(result.2, BLOCK + 5 + 2);
}

/// SPEC 18.2.3: what a device holds is counted from its records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_contents_count_versions_keys_blocks_and_bytes() {
    let (a, b) = two_nodes().await;
    let mut client = Client::connect(ClientOptions::new(vec![a.addr]).cluster(a.node.cluster_id()))
        .await
        .expect("connect");
    let device_a = a.node.devices()[0].id();
    let device_b = b.node.devices()[0].id();
    let empty = client.device_contents(device_a).await.expect("contents");
    assert_eq!(
        (empty.versions, empty.keys, empty.blocks, empty.shard_bytes),
        (0, 0, 0, 0)
    );
    assert_eq!(empty.node, a.node.id());

    // Two keys, one written twice: the second write replaces the first
    // version, so two versions remain, and 1+1 puts a shard of each on
    // both devices.
    let body: Vec<u8> = vec![1u8; 2 * BLOCK as usize + 7];
    client.put("k", &body, None).await.expect("put");
    client.put("k", &body, None).await.expect("put again");
    client.put("other", b"x", None).await.expect("put other");
    for device in [device_a, device_b] {
        let contents = client.device_contents(device).await.expect("contents");
        assert_eq!(contents.versions, 2, "{contents:?}");
        assert_eq!(contents.keys, 2);
        // k spans three blocks (two full and a short one), `other` one.
        assert_eq!(contents.blocks, 3 + 1);
        assert!(contents.shard_bytes > 2 * BLOCK + 7, "{contents:?}");
    }
    let unknown = client
        .device_contents(djbod_core::record::DeviceId(Uuid::new_v4()))
        .await
        .expect_err("unknown device");
    assert!(unknown.is_not_found(), "{unknown}");
}

/// One node with `count` devices, for operations that need spare devices.
async fn one_node(count: usize, k: u8, m: u8) -> TestNode {
    let (listener, addr) = reserve_port().await;
    let dirs: Vec<tempfile::TempDir> = (0..count)
        .map(|_| tempfile::tempdir().expect("temp dir"))
        .collect();
    let state = tempfile::tempdir().expect("temp dir");
    let config = NodeConfig {
        node_id: Uuid::new_v4(),
        listen: addr,
        advertise: None,
        state_dir: state.path().to_path_buf(),
        devices: dirs.iter().map(|d| d.path().to_path_buf()).collect(),
        bootstrap_peers: vec![],
        temporary_max_age_secs: 3600,
        stream_idle_timeout_secs: 120,
        allow_shared_filesystem: true,
        tls: None,
    };
    let node = Arc::new(
        Node::init_cluster(
            config,
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
    TestNode {
        _server: tokio::spawn(server::serve(node.clone(), listener)),
        node,
        addr,
        _dirs: dirs,
        _state: state,
    }
}

/// The client's move-shard, scrub and drain: the last conversations that
/// used to need a bare connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_shard_scrub_and_drain_are_client_methods() {
    use djbod_proto::message::{DrainEvent, ScrubEvent};
    let a = one_node(3, 1, 1).await;
    let cluster = a.node.cluster_id();
    let mut client = Client::connect(ClientOptions::new(vec![a.addr]).cluster(cluster))
        .await
        .expect("connect");
    let body: Vec<u8> = vec![3u8; BLOCK as usize + 1];
    client.put("k", &body, None).await.expect("put");
    let record = client.head("k").await.expect("head");
    let listed: Vec<_> = record.shards.iter().map(|s| s.device).collect();
    let spare = a
        .node
        .devices()
        .iter()
        .map(|d| d.id())
        .find(|d| !listed.contains(d))
        .expect("a device without a shard");

    // Move shard 0 to the spare device.
    let moved = client
        .move_shard("k", 0, Some(spare))
        .await
        .expect("move shard");
    assert_eq!(moved.source, listed[0]);
    assert_eq!(moved.record.shards[0].device, spare);
    assert_eq!(moved.record.revision, record.revision + 1);
    assert!(moved.source_cleaned && !moved.rebuilt);

    // A scrub: one summary per device, then a clean end.
    let mut run = client.scrub(None, false).await.expect("start scrub");
    let mut summaries = 0;
    let end = loop {
        match run.next_event().await.expect("scrub event") {
            Ok(ScrubEvent::NodeSummary { .. }) => summaries += 1,
            Ok(other) => panic!("unexpected scrub event {other:?}"),
            Err(end) => break end,
        }
    };
    assert_eq!(summaries, 3);
    assert!(end.error.is_none(), "{end:?}");

    // Drain the device shard 0 now sits on: an estimate, one move, an end.
    djbod_client::admin::set_device_state(
        &djbod_client::transport::Connector::plain(),
        a.addr,
        cluster,
        spare,
        djbod_core::cluster::DeviceState::Draining,
    )
    .await
    .expect("set draining");
    let mut run = client.drain(spare, false).await.expect("start drain");
    let mut events = Vec::new();
    let end = loop {
        match run.next_event().await.expect("drain event") {
            Ok(event) => events.push(event),
            Err(end) => break end,
        }
    };
    assert!(
        matches!(events[0], DrainEvent::Estimate { versions: 1, .. }),
        "{events:?}"
    );
    assert!(matches!(events[1], DrainEvent::Moved { .. }), "{events:?}");
    assert!(end.error.is_none(), "{end:?}");
    let contents = client.device_contents(spare).await.expect("contents");
    assert_eq!(contents.versions, 0, "drained empty");
    // The client is usable again after the runs took its connection.
    assert_eq!(client.get("k").await.expect("get").1, body);
}
