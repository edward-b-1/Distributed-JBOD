//! The client library (SPEC 20.8) against nodes in this process: learning
//! the cluster id, every operation, failover to another node, and the
//! blocking facade.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;

use djbod_client::{blocking, Client, ClientOptions, Error};
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
    let (record, got) = client.get("k").await.expect("get");
    assert_eq!(got, body);
    assert_eq!(record.version, version);
    assert_eq!(
        record.content_type.as_deref(),
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
    assert_eq!(identity.build.as_deref(), Some(djbod_client::BUILD));
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
        Err(Error::NoNodes)
    ));
    let (_unused, dead) = reserve_port().await;
    drop(_unused);
    match Client::connect(ClientOptions::new(vec![dead])).await {
        Err(Error::Unreachable(attempts)) => assert_eq!(attempts.len(), 1),
        other => panic!("expected unreachable, got {:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_blocking_client_does_the_same_without_async() {
    let (a, _b) = two_nodes().await;
    let options = ClientOptions::new(vec![a.addr]).cluster(a.node.cluster_id());
    // Its own runtime, so it runs on a plain thread, as a binding would.
    let result = std::thread::spawn(move || -> Result<(usize, Vec<u8>, u64), Error> {
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
        let record = client.get_to_writer("two", &mut sink)?;
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
