//! The command-line client against a real node started in-process.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use tokio::net::TcpListener;
use uuid::Uuid;

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
        block_size: 64 * 1024,
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

/// Run the `djbod` binary against the test node. Returns (status ok,
/// raw stdout, stderr).
fn djbod_raw(test: &TestNode, args: &[&str]) -> (bool, Vec<u8>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_djbod"))
        .arg("--node")
        .arg(test.addr.to_string())
        .arg("--cluster")
        .arg(test.node.cluster_id().to_string())
        .args(args)
        .output()
        .expect("run djbod");
    (
        output.status.success(),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// As `djbod_raw`, with stdout as text.
fn djbod(test: &TestNode, args: &[&str]) -> (bool, String, String) {
    let (ok, out, err) = djbod_raw(test, args);
    (ok, String::from_utf8_lossy(&out).into_owned(), err)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_get_head_list_delete_from_the_command_line() {
    let test = start_node(4, 3, 1).await;
    let work = tempfile::tempdir().expect("temp dir");
    let body = xorshift64_bytes(3 * 3 * 64 * 1024 + 4321, 1);
    let input = work.path().join("input.bin");
    std::fs::write(&input, &body).expect("write input");

    let (ok, out, err) = djbod(&test, &["status"]);
    assert!(ok, "status failed: {err}");
    assert!(out.contains("DEVICE"), "{out}");
    assert_eq!(out.matches("active").count(), 4, "{out}");

    let (ok, out, err) = djbod(
        &test,
        &[
            "put",
            "docs/report.pdf",
            input.to_str().unwrap(),
            "--content-type",
            "application/pdf",
        ],
    );
    assert!(ok, "put failed: {err}");
    assert!(
        out.starts_with("stored docs/report.pdf as version "),
        "{out}"
    );

    let (ok, out, err) = djbod(&test, &["head", "docs/report.pdf"]);
    assert!(ok, "head failed: {err}");
    assert!(
        out.contains(&format!("size          {} bytes", body.len())),
        "{out}"
    );
    assert!(out.contains("content-type  application/pdf"), "{out}");
    assert!(out.contains("scheme        3+1"), "{out}");

    let (ok, out, err) = djbod(&test, &["--json", "head", "docs/report.pdf"]);
    assert!(ok, "head --json failed: {err}");
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(record["key"], "docs/report.pdf");
    assert_eq!(record["shards"].as_array().expect("shards").len(), 4);

    let output = work.path().join("output.bin");
    let (ok, _, err) = djbod(&test, &["get", "docs/report.pdf", output.to_str().unwrap()]);
    assert!(ok, "get failed: {err}");
    assert_eq!(std::fs::read(&output).expect("read output"), body);

    // To standard output, byte for byte.
    let (ok, out, err) = djbod_raw(&test, &["get", "docs/report.pdf"]);
    assert!(ok, "get to stdout failed: {err}");
    assert!(out == body, "stdout body differs from input");

    let (ok, out, err) = djbod(&test, &["list", "--prefix", "docs/"]);
    assert!(ok, "list failed: {err}");
    assert!(out.contains("docs/report.pdf"), "{out}");
    assert!(out.contains(&format!("{:>14}", body.len())), "{out}");

    let (ok, out, err) = djbod(&test, &["delete", "docs/report.pdf"]);
    assert!(ok, "delete failed: {err}");
    assert_eq!(out.trim(), "deleted docs/report.pdf");

    let (ok, out, err) = djbod(&test, &["head", "docs/report.pdf"]);
    assert!(!ok);
    assert!(out.is_empty());
    assert!(err.contains("NotFound"), "{err}");
    assert!(err.contains("key:     docs/report.pdf"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_get_removes_the_partial_output_file() {
    let test = start_node(2, 1, 1).await;
    let work = tempfile::tempdir().expect("temp dir");
    let body = xorshift64_bytes(4 * 64 * 1024, 2);
    let input = work.path().join("input.bin");
    std::fs::write(&input, &body).expect("write input");
    let (ok, _, err) = djbod(&test, &["put", "k", input.to_str().unwrap()]);
    assert!(ok, "put failed: {err}");

    // Corrupt block 3 of shard 0 on disk (1+1 replication: shard 0 is the
    // only data shard).
    let (_, out, _) = djbod(&test, &["--json", "head", "k"]);
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    let device0 = record["shards"][0]["device"]
        .as_str()
        .expect("device")
        .to_string();
    let version = record["version"].as_str().expect("version");
    let device = test
        .node
        .devices()
        .into_iter()
        .find(|d| d.id().0.to_string() == device0)
        .expect("device");
    let shard = device
        .object_directory(&djbod_core::keyhash::hash_key(b"k"))
        .join(format!("{version}.0.shard"));
    let mut bytes = std::fs::read(&shard).expect("read shard");
    bytes[4096 + 3 * 64 * 1024 + 5] ^= 0x01;
    std::fs::write(&shard, &bytes).expect("write shard");

    let output = work.path().join("output.bin");
    let (ok, _, err) = djbod(&test, &["get", "k", output.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("BlockChecksumMismatch"), "{err}");
    assert!(err.contains("stripe:  3"), "{err}");
    assert!(err.contains("partial output"), "{err}");
    assert!(!output.exists(), "partial output should have been removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_connection_details_are_explained() {
    let output = Command::new(env!("CARGO_BIN_EXE_djbod"))
        .env_remove("DJBOD_NODE")
        .env_remove("DJBOD_CLUSTER")
        .arg("status")
        .output()
        .expect("run djbod");
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("--node"), "{err}");
}
