//! `djbod-node scrub` run as a process against a node's devices while the
//! node runs in-process.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use djbod_core::keyhash::hash_key;
use djbod_core::layout::shard_file_name;
use djbod_node::client::Connection;
use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_proto::message::{Request, Response};
use tokio::net::TcpListener;
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

struct TestNode {
    node: Arc<Node>,
    addr: SocketAddr,
    config_path: std::path::PathBuf,
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
    let config_path = state.path().join("node.toml");
    std::fs::write(&config_path, config.to_toml()).expect("write config");
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
        config_path,
        _dirs: dirs,
        _state: state,
    }
}

fn scrub(test: &TestNode, extra: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_djbod-node"))
        .arg("scrub")
        .arg("--config")
        .arg(&test.config_path)
        .args(extra)
        .output()
        .expect("run djbod-node scrub");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scrub_finds_damage_and_exits_nonzero() {
    let test = start_node(4, 3, 1).await;
    let mut client =
        Connection::connect(test.addr, Connection::client_hello(test.node.cluster_id()))
            .await
            .expect("connect");
    let body = xorshift64_bytes(4 * 3 * BLOCK as usize + 5, 1);
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

    // Clean: exit 0, no findings, totals on stderr.
    let (code, out, err) = scrub(&test, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("4 shards, 16 blocks") || err.contains("total: 4 records"),
        "{err}"
    );

    // Corrupt one block on one device.
    let device = record
        .device_for(djbod_core::erasure::ShardIndex(1))
        .expect("device");
    let path = test
        .node
        .device(device)
        .expect("device")
        .object_directory(&hash_key(b"k"))
        .join(shard_file_name(
            &record.version,
            djbod_core::erasure::ShardIndex(1),
        ));
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[4096 + 2 * BLOCK as usize + 9] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");

    // Found, as JSON, exit 2.
    let (code, out, _) = scrub(&test, &["--json"]);
    assert_eq!(code, 2);
    let finding: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one finding")).expect("json");
    assert_eq!(finding["kind"], "shard_blocks_corrupt");
    assert_eq!(finding["key"], "k");
    assert_eq!(finding["shard_index"], 1);
    assert_eq!(finding["stripes"], serde_json::json!([2]));

    // Only the damaged device, in text.
    let (code, out, _) = scrub(
        &test,
        &[
            "--device",
            path.ancestors().nth(6).unwrap().to_str().unwrap(),
        ],
    );
    assert_eq!(code, 2);
    assert!(out.contains("shard blocks corrupt"), "{out}");

    // Repair through the node, then a second scrub is clean.
    match client
        .request(Request::RepairObject {
            key: "k".to_string(),
        })
        .await
        .expect("repair")
    {
        Response::RepairObject(report) => {
            assert_eq!(report.shards.iter().filter(|s| s.rewritten).count(), 1)
        }
        other => panic!("{other:?}"),
    }
    let (code, out, _) = scrub(&test, &[]);
    assert_eq!(code, 0, "{out}");
    let (_, got) = client.get_object("k").await.expect("get");
    assert_eq!(got, body);
}
