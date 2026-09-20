//! The command-line client against a real node started in-process.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_node::transport::TlsPaths;
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
    start_node_with_tls(device_count, k, m, None).await
}

async fn start_node_with_tls(device_count: usize, k: u8, m: u8, tls: Option<TlsPaths>) -> TestNode {
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
        allow_shared_filesystem: true,
        tls,
    };
    let parameters = ClusterParameters {
        k,
        m,
        block_size: 64 * 1024,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_state_and_drain_from_the_command_line() {
    let test = start_node(5, 3, 1).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("in.bin");
    std::fs::write(&source, xorshift64_bytes(300_000, 5)).expect("write");
    let (ok, _, err) = djbod(&test, &["put", "k", source.to_str().unwrap()]);
    assert!(ok, "{err}");
    let (ok, out, err) = djbod(&test, &["--json", "head", "k"]);
    assert!(ok, "{err}");
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    let device = record["shards"][0]["device"]
        .as_str()
        .expect("device")
        .to_string();

    let (ok, out, err) = djbod(&test, &["cluster", "drain", &device]);
    assert!(!ok, "{out}");
    assert!(err.contains("set-state"), "{err}");

    let (ok, out, err) = djbod(&test, &["cluster", "set-state", &device, "draining"]);
    assert!(ok, "{err}");
    assert!(out.contains("is now draining"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "set-state", &device, "draining"]);
    assert!(ok);
    assert!(out.contains("already draining"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(out.contains("draining"), "{out}");

    let (ok, out, err) = djbod(&test, &["cluster", "drain", &device]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("1 version(s)"), "{out}");
    assert!(out.contains("moved    k  shard 0 -> "), "{out}");
    assert!(err.contains("1 moved, 0 skipped"), "{err}");
    let (ok, out, err) = djbod(&test, &["--json", "head", "k"]);
    assert!(ok, "{err}");
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(record["revision"], 1);
    assert_ne!(
        record["shards"][0]["device"].as_str().expect("device"),
        device
    );

    let (ok, out, _) = djbod(&test, &["cluster", "set-state", &device, "active"]);
    assert!(ok);
    assert!(out.contains("is now active"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_device_from_the_command_line() {
    let test = start_node(5, 3, 1).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("in.bin");
    std::fs::write(&source, xorshift64_bytes(200_000, 6)).expect("write");
    let (ok, _, err) = djbod(&test, &["put", "k", source.to_str().unwrap()]);
    assert!(ok, "{err}");
    let (ok, out, _) = djbod(&test, &["--json", "head", "k"]);
    assert!(ok);
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    let device = record["shards"][1]["device"]
        .as_str()
        .expect("device")
        .to_string();

    let (ok, _, err) = djbod(&test, &["cluster", "remove-device", &device]);
    assert!(!ok);
    assert!(err.contains("is active"), "{err}");
    assert!(err.contains("set-state"), "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-state", &device, "draining"]);
    assert!(ok, "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "remove-device", &device]);
    assert!(!ok);
    assert!(
        err.contains("still named by the current record of 1 version(s)"),
        "{err}"
    );
    assert!(err.contains("\"k\""), "{err}");

    let (ok, _, err) = djbod(&test, &["cluster", "set-state", &device, "draining"]);
    assert!(ok, "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "drain", &device]);
    assert!(ok, "{err}");
    let (ok, out, err) = djbod(&test, &["cluster", "remove-device", &device]);
    assert!(ok, "{err}");
    assert!(out.contains("removed (document version 3)"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(out.contains("removed"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "remove-device", &device]);
    assert!(ok);
    assert!(out.contains("already removed"), "{out}");

    // The only node cannot be removed.
    let (ok, _, err) = djbod(
        &test,
        &["cluster", "remove-node", &test.node.id().0.to_string()],
    );
    assert!(!ok);
    assert!(err.contains("only node"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_scheme_changes_the_document_and_reencode_rewrites_the_objects() {
    let test = start_node(6, 3, 1).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let big = dir.path().join("big.bin");
    let small = dir.path().join("small.bin");
    let empty = dir.path().join("empty.bin");
    std::fs::write(&big, xorshift64_bytes(2 * 3 * 64 * 1024 + 777, 11)).expect("write");
    std::fs::write(&small, b"just a few bytes").expect("write");
    std::fs::write(&empty, b"").expect("write");
    for (key, path, extra) in [
        (
            "big",
            &big,
            &["--content-type", "application/octet-stream"][..],
        ),
        ("small", &small, &[][..]),
        ("empty", &empty, &[][..]),
    ] {
        let (ok, _, err) = djbod(
            &test,
            &[&["put", key, path.to_str().unwrap()][..], extra].concat(),
        );
        assert!(ok, "{err}");
    }

    // Too few active devices for the new scheme: refused, nothing changes.
    let (ok, _, err) = djbod(&test, &["cluster", "set-scheme", "--k", "5", "--m", "2"]);
    assert!(!ok);
    assert!(
        err.contains("6 active device(s) but scheme 5+2 needs 7"),
        "{err}"
    );

    // The scheme change moves no data: the objects stay at 3+1, readable.
    let (ok, out, err) = djbod(&test, &["cluster", "set-scheme", "--k", "4", "--m", "2"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("scheme is now 4+2"), "{out}");
    assert!(
        out.contains("3 object(s) are stored at another scheme"),
        "{out}"
    );
    let (ok, out, _) = djbod(&test, &["--json", "head", "big"]);
    assert!(ok);
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(record["k"], 3);
    let copy = dir.path().join("big.before");
    let (ok, _, err) = djbod(&test, &["get", "big", copy.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert_eq!(
        std::fs::read(&copy).expect("read"),
        std::fs::read(&big).expect("read")
    );

    let (ok, out, err) = djbod(&test, &["cluster", "reencode"]);
    assert!(ok, "{out}{err}");
    assert_eq!(out.matches("re-encoded  ").count(), 3, "{out}");
    assert!(out.contains("re-encoded  big  3+1 -> 4+2"), "{out}");
    assert!(
        err.contains("3 object(s) examined, 3 re-encoded, 0 failed"),
        "{err}"
    );

    for (key, path) in [("big", &big), ("small", &small), ("empty", &empty)] {
        let (ok, out, err) = djbod(&test, &["--json", "head", key]);
        assert!(ok, "{err}");
        let record: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(record["k"], 4, "{key}: {out}");
        assert_eq!(record["m"], 2, "{key}: {out}");
        assert_eq!(record["shards"].as_array().expect("shards").len(), 6);
        if key == "big" {
            assert_eq!(record["content_type"], "application/octet-stream");
        }
        let copy = dir.path().join(format!("{key}.copy"));
        let (ok, _, err) = djbod(&test, &["get", key, copy.to_str().unwrap()]);
        assert!(ok, "{err}");
        assert_eq!(
            std::fs::read(&copy).expect("read"),
            std::fs::read(path).expect("read")
        );
    }
    // Only the newest version's files remain on each device.
    for device in test.node.devices() {
        for key_dir in device.key_directories().expect("list") {
            let names: Vec<String> = std::fs::read_dir(&key_dir)
                .expect("read dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                names.iter().filter(|n| n.ends_with(".meta.json")).count(),
                1,
                "{names:?}"
            );
        }
    }
    let (ok, out, err) = djbod(&test, &["scrub"]);
    assert!(ok, "{out}{err}");

    // Reruns change nothing and re-encode nothing.
    let (ok, out, err) = djbod(&test, &["cluster", "set-scheme", "--k", "4", "--m", "2"]);
    assert!(ok, "{err}");
    assert!(out.contains("was already 4+2"), "{out}");
    assert!(out.contains("every object is at this scheme"), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "reencode"]);
    assert!(ok, "{err}");
    assert!(
        err.contains("3 object(s) examined, 0 re-encoded, 0 failed"),
        "{err}"
    );

    // New writes use the new scheme.
    let (ok, _, err) = djbod(&test, &["put", "later", small.to_str().unwrap()]);
    assert!(ok, "{err}");
    let (ok, out, _) = djbod(&test, &["--json", "head", "later"]);
    assert!(ok);
    let record: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(record["k"], 4);
    assert_eq!(record["m"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_limits_from_the_command_line() {
    let test = start_node(2, 1, 1).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let big = dir.path().join("big.bin");
    std::fs::write(&big, [1u8; 5000]).expect("write");

    let (ok, out, err) = djbod(
        &test,
        &["cluster", "set-limits", "--max-object-bytes", "4096"],
    );
    assert!(ok, "{err}");
    assert!(
        out.contains(
            "max object size 4096 bytes, max user metadata 10485760 bytes (document version 2)"
        ),
        "{out}"
    );
    let (ok, _, err) = djbod(&test, &["put", "big", big.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("ObjectTooLarge"), "{err}");
    assert!(err.contains("4096"), "{err}");

    let (ok, out, err) = djbod(&test, &["cluster", "set-limits", "--max-key-bytes", "3"]);
    assert!(ok, "{err}");
    assert!(out.contains("max key length 3 bytes"), "{out}");
    let (ok, _, err) = djbod(&test, &["head", "four"]);
    assert!(!ok);
    assert!(err.contains("KeyTooLong"), "{err}");
    let (ok, out, err) = djbod(&test, &["cluster", "set-limits", "--max-key-bytes", "3"]);
    assert!(ok, "{err}");
    assert!(out.contains("nothing changed"), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-limits", "--max-key-bytes", "0"]);
    assert!(!ok);
    assert!(
        err.contains("max_key_bytes 0 must be between 1 and"),
        "{err}"
    );
    let (ok, _, _) = djbod(&test, &["cluster", "set-limits"]);
    assert!(!ok, "one of the three flags is required");
    let (ok, out, err) = djbod(
        &test,
        &[
            "cluster",
            "set-limits",
            "--max-user-metadata-bytes",
            "1000000",
        ],
    );
    assert!(ok, "{err}");
    assert!(out.contains("max user metadata 1000000 bytes"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster-config"]);
    assert!(ok);
    assert!(out.contains("\"max_key_bytes\": 3"), "{out}");
    assert!(out.contains("\"max_object_bytes\": 4096"), "{out}");
    assert!(
        out.contains("\"max_user_metadata_bytes\": 1000000"),
        "{out}"
    );
}

/// A certificate authority for the TLS test, issuing PEM files the way an
/// administrator would with openssl (SPEC 19.1.6.1).
struct Authority {
    dir: tempfile::TempDir,
    ca_cert: rcgen::Certificate,
    ca_key: rcgen::KeyPair,
    issued: usize,
}

impl Authority {
    fn new() -> Authority {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "djbod cli test CA");
        let ca_key = KeyPair::generate().expect("key");
        let ca_cert = params.self_signed(&ca_key).expect("self-signed");
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("ca.crt"), ca_cert.pem()).expect("write");
        Authority {
            dir,
            ca_cert,
            ca_key,
            issued: 0,
        }
    }

    fn ca(&self) -> String {
        self.dir.path().join("ca.crt").to_str().unwrap().to_string()
    }

    fn issue(&mut self, host: &str) -> TlsPaths {
        use std::os::unix::fs::PermissionsExt;
        self.issued += 1;
        let params = rcgen::CertificateParams::new(vec![host.to_string()]).expect("params");
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params
            .signed_by(&key, &self.ca_cert, &self.ca_key)
            .expect("signed");
        let cert_path = self.dir.path().join(format!("{}.crt", self.issued));
        let key_path = self.dir.path().join(format!("{}.key", self.issued));
        std::fs::write(&cert_path, cert.pem()).expect("write");
        std::fs::write(&key_path, key.serialize_pem()).expect("write");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        TlsPaths {
            cert: cert_path,
            key: key_path,
            ca: self.dir.path().join("ca.crt"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_speaks_tls_with_flags_or_environment() {
    let mut authority = Authority::new();
    let test = start_node_with_tls(2, 1, 1, Some(authority.issue("127.0.0.1"))).await;
    let admin = authority.issue("admin-laptop");
    let ca = authority.ca();
    let cert = admin.cert.to_str().unwrap();
    let key = admin.key.to_str().unwrap();
    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("in.bin");
    std::fs::write(&input, xorshift64_bytes(100_000, 8)).expect("write");

    // Plain cluster: plain and TLS clients both work.
    let (ok, out, err) = djbod(&test, &["status"]);
    assert!(ok, "{err}");
    assert!(out.contains("transport plain"), "{out}");
    let (ok, _, err) = djbod(
        &test,
        &[
            "--tls-ca",
            &ca,
            "--tls-cert",
            cert,
            "--tls-key",
            key,
            "put",
            "k",
            input.to_str().unwrap(),
        ],
    );
    assert!(ok, "{err}");

    // tls: the plain client is told why it was refused; --tls-ca alone
    // (no client certificate) is refused too; the full identity works,
    // as flags and as environment variables.
    let (ok, _, err) = djbod(&test, &["cluster", "set-transport", "tls"]);
    assert!(ok, "{err}");
    let (ok, _, err) = djbod(&test, &["status"]);
    assert!(!ok);
    assert!(err.contains("TlsRequired"), "{err}");
    let (ok, _, err) = djbod(&test, &["--tls-ca", &ca, "status"]);
    assert!(
        !ok,
        "an anonymous TLS client must be refused under transport tls"
    );
    assert!(!err.is_empty());
    let (ok, out, err) = djbod(
        &test,
        &[
            "--tls-ca",
            &ca,
            "--tls-cert",
            cert,
            "--tls-key",
            key,
            "status",
        ],
    );
    assert!(ok, "{err}");
    assert!(out.contains("transport tls"), "{out}");
    let copy = dir.path().join("copy.bin");
    let output = Command::new(env!("CARGO_BIN_EXE_djbod"))
        .env("DJBOD_NODE", test.addr.to_string())
        .env("DJBOD_CLUSTER", test.node.cluster_id().to_string())
        .env("DJBOD_TLS_CA", &ca)
        .env("DJBOD_TLS_CERT", cert)
        .env("DJBOD_TLS_KEY", key)
        .args(["get", "k", copy.to_str().unwrap()])
        .output()
        .expect("run djbod");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&copy).expect("read"),
        std::fs::read(&input).expect("read")
    );
    // --tls-cert without --tls-key is a usage error.
    let (ok, _, err) = djbod(&test, &["--tls-ca", &ca, "--tls-cert", cert, "status"]);
    assert!(!ok);
    assert!(err.contains("--tls-key"), "{err}");

    // tls-optional: an anonymous TLS client is accepted, and so is plain.
    let (ok, _, err) = djbod(
        &test,
        &[
            "--tls-ca",
            &ca,
            "--tls-cert",
            cert,
            "--tls-key",
            key,
            "cluster",
            "set-transport",
            "tls-optional",
        ],
    );
    assert!(ok, "{err}");
    let (ok, out, err) = djbod(&test, &["--tls-ca", &ca, "status"]);
    assert!(ok, "{err}");
    assert!(out.contains("transport tls-optional"), "{out}");
    let (ok, _, err) = djbod(&test, &["head", "k"]);
    assert!(ok, "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_can_be_labelled_and_named_by_label() {
    // Five devices at 3+1, so one can drain while four stay active.
    let test = start_node(5, 3, 1).await;
    let device = test.node.devices()[1].id().0.to_string();
    let other = test.node.devices()[2].id().0.to_string();

    let (ok, out, err) = djbod(&test, &["cluster", "set-label", &device, "nas1-bay1"]);
    assert!(ok, "{err}");
    assert!(out.contains("is now labelled nas1-bay1"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "set-label", &device, "nas1-bay1"]);
    assert!(ok);
    assert!(out.contains("nothing changed"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(out.contains("LABEL"), "{out}");
    assert!(out.contains("nas1-bay1"), "{out}");

    // A duplicate, a label with whitespace, and one that looks like a
    // UUID are refused by the document validator.
    let (ok, _, err) = djbod(&test, &["cluster", "set-label", &other, "nas1-bay1"]);
    assert!(!ok);
    assert!(err.contains("used by more than one device"), "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-label", &other, "bay 2"]);
    assert!(!ok);
    assert!(err.contains("whitespace"), "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-label", &other, &device]);
    assert!(!ok);
    assert!(err.contains("looks like a UUID"), "{err}");

    // Every command that takes a device takes the label instead.
    let (ok, out, err) = djbod(&test, &["cluster", "set-state", "nas1-bay1", "draining"]);
    assert!(ok, "{err}");
    assert!(out.contains(&device), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "drain", "nas1-bay1"]);
    assert!(ok, "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-state", "nas1-bay1", "active"]);
    assert!(ok, "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-state", "no-such-label", "active"]);
    assert!(!ok);
    assert!(
        err.contains("no device is named \"no-such-label\""),
        "{err}"
    );

    // Relabel by the current label, then clear.
    let (ok, _, err) = djbod(&test, &["cluster", "set-label", "nas1-bay1", "nas1-bay9"]);
    assert!(ok, "{err}");
    let (ok, out, err) = djbod(&test, &["cluster", "set-label", "nas1-bay9", "--clear"]);
    assert!(ok, "{err}");
    assert!(out.contains("label cleared"), "{out}");
    let (ok, out, _) = djbod(&test, &["--json", "cluster-config"]);
    assert!(ok);
    assert!(!out.contains("\"label\""), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_can_be_labelled_and_named_by_label() {
    let test = start_node(5, 3, 1).await;
    let node_id = test.node.id().0.to_string();
    let device = test.node.devices()[0].id().0.to_string();

    let (ok, out, err) = djbod(&test, &["cluster", "set-node-label", &node_id, "nas1"]);
    assert!(ok, "{err}");
    assert!(out.contains("is now labelled nas1"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "set-node-label", "nas1", "nas1"]);
    assert!(ok);
    assert!(out.contains("nothing changed"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(out.contains("NODE LABEL"), "{out}");
    assert!(out.contains("nas1"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "show"]);
    assert!(ok);
    assert!(out.contains("nas1"), "{out}");
    // Each node's build, for telling an older node apart (SPEC 6.2.6.4).
    assert!(out.contains("BUILD"), "{out}");
    assert!(out.contains(djbod_node::BUILD), "{out}");
    let (ok, out, _) = djbod(&test, &["--version"]);
    assert!(ok);
    assert!(out.contains(djbod_node::BUILD), "{out}");

    // A device may carry the same label as a node: separate namespaces.
    let (ok, _, err) = djbod(&test, &["cluster", "set-label", &device, "nas1"]);
    assert!(ok, "{err}");
    // A second node could not, but there is only one; a bad label is
    // refused the same way as for devices.
    let (ok, _, err) = djbod(&test, &["cluster", "set-node-label", "nas1", "has space"]);
    assert!(!ok);
    assert!(err.contains("whitespace"), "{err}");

    // Commands that take a node take the label: drain --node-id finds no
    // draining devices, and remove-node is refused as the only node,
    // which shows the name resolved.
    let (ok, out, err) = djbod(&test, &["cluster", "drain", "--node-id", "nas1"]);
    assert!(ok, "{err}");
    assert!(out.contains("has no draining devices"), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "remove-node", "nas1"]);
    assert!(!ok);
    assert!(err.contains("only node"), "{err}");
    let (ok, _, err) = djbod(&test, &["cluster", "remove-node", "no-such-node"]);
    assert!(!ok);
    assert!(err.contains("no node is named \"no-such-node\""), "{err}");

    let (ok, out, err) = djbod(&test, &["cluster", "set-node-label", "nas1", "--clear"]);
    assert!(ok, "{err}");
    assert!(out.contains("label cleared"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nodes_address_can_be_set_from_the_command_line() {
    let test = start_node(2, 1, 1).await;
    let node_id = test.node.id().0.to_string();
    let here = test.addr.to_string();

    let (ok, out, err) = djbod(&test, &["cluster", "set-address", &node_id, &here]);
    assert!(ok, "{err}");
    assert!(out.contains("nothing changed"), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-address", &node_id, "nowhere"]);
    assert!(!ok);
    assert!(err.contains("not an IP address and port"), "{err}");
    let twice = format!("{here},{here}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-address", &node_id, &twice]);
    assert!(!ok);
    assert!(err.contains("more than one node"), "{err}");

    // A second address, after the one the node is reached at.
    let list = format!("{here},127.0.0.1:1");
    let (ok, out, err) = djbod(&test, &["cluster", "set-address", &node_id, &list]);
    assert!(ok, "{err}");
    assert!(out.contains("is now reached at"), "{out}");
    assert!(out.contains("127.0.0.1:1"), "{out}");
    let (ok, out, _) = djbod(&test, &["cluster", "show"]);
    assert!(ok);
    assert!(out.contains("127.0.0.1:1"), "{out}");
    assert_eq!(
        test.node.document().nodes[0].addresses,
        vec![here.clone(), "127.0.0.1:1".to_string()]
    );
    let (ok, _, err) = djbod(&test, &["cluster", "set-address", "no-such-node", &here]);
    assert!(!ok);
    assert!(err.contains("no node is named"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cluster_can_be_named_from_the_command_line() {
    let test = start_node(2, 1, 1).await;
    let cluster = test.node.cluster_id().to_string();

    let (ok, out, err) = djbod(&test, &["cluster", "set-name", "Home NAS"]);
    assert!(ok, "{err}");
    assert!(out.contains("is now named Home NAS"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(
        out.contains(&format!("cluster   Home NAS ({cluster})")),
        "{out}"
    );
    let (ok, out, _) = djbod(&test, &["cluster", "show"]);
    assert!(ok);
    assert!(
        out.contains(&format!("cluster   Home NAS ({cluster})")),
        "{out}"
    );
    let (ok, out, _) = djbod(&test, &["--json", "status"]);
    assert!(ok);
    assert!(out.contains("\"cluster_name\": \"Home NAS\""), "{out}");

    let (ok, out, _) = djbod(&test, &["cluster", "set-name", "Home NAS"]);
    assert!(ok);
    assert!(out.contains("nothing changed"), "{out}");
    let (ok, _, err) = djbod(&test, &["cluster", "set-name", " padded"]);
    assert!(!ok);
    assert!(err.contains("start or end with whitespace"), "{err}");

    let (ok, out, err) = djbod(&test, &["cluster", "set-name", "--clear"]);
    assert!(ok, "{err}");
    assert!(out.contains("name cleared"), "{out}");
    let (ok, out, _) = djbod(&test, &["status"]);
    assert!(ok);
    assert!(out.contains(&format!("cluster   {cluster}\n")), "{out}");
}
