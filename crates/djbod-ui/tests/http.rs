//! The web UI's API against a real node started in-process.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use djbod_client::transport::Connector;
use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;
use djbod_ui::{router, router_for_hosts, Target};
use tokio::net::TcpListener;

struct TestNode {
    node: Arc<Node>,
    addr: SocketAddr,
    dirs: Vec<tempfile::TempDir>,
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
        allow_shared_filesystem: true,
        tls: None,
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
        dirs,
        _state: state,
    }
}

fn app(test: &TestNode) -> axum::Router {
    router(Target {
        node: test.addr,
        cluster: test.node.cluster_id(),
        connector: Connector::plain(),
    })
}

fn pattern_bytes(len: usize, seed: u64) -> Vec<u8> {
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

/// Send one request and return the status, headers, and whole body.
async fn call(
    test: &TestNode,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Bytes) {
    let response = app(test).oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, body)
}

async fn get_json(test: &TestNode, path: &str) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = call(
        test,
        Request::get(path).body(Body::empty()).expect("request"),
    )
    .await;
    (status, serde_json::from_slice(&body).expect("json body"))
}

async fn post_json(
    test: &TestNode,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = call(
        test,
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
    )
    .await;
    (status, serde_json::from_slice(&body).expect("json body"))
}

async fn put_object(
    test: &TestNode,
    key: &str,
    body: &[u8],
    content_type: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request =
        Request::put(format!("/api/objects/{key}")).header(header::CONTENT_LENGTH, body.len());
    if let Some(ct) = content_type {
        request = request.header(header::CONTENT_TYPE, ct);
    }
    let (status, _, out) = call(
        test,
        request.body(Body::from(body.to_vec())).expect("request"),
    )
    .await;
    (status, serde_json::from_slice(&out).expect("json body"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_the_page() {
    let test = start_node(4, 3, 1).await;
    let (status, headers, body) = call(
        &test,
        Request::get("/").body(Body::empty()).expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("<title>Distributed-JBOD</title>"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_and_cluster_describe_the_node() {
    let test = start_node(4, 3, 1).await;
    let (status, json) = get_json(&test, "/api/status").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["cluster_id"], test.node.cluster_id().to_string());
    assert!(json["cluster_name"].is_null(), "unnamed: {json}");
    assert_eq!(json["devices"].as_array().unwrap().len(), 4);
    assert_eq!(json["devices"][0]["state"], "active");
    assert_eq!(json["transport"], "plain");
    assert_eq!(json["ui_to_node_tls"], false);
    assert_eq!(json["ui_build"], djbod_client::BUILD);

    let (status, json) = get_json(&test, "/api/cluster").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["document"]["k"], 3);
    assert_eq!(json["document"]["m"], 1);
    let nodes = json["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["version"], json["document"]["version"]);
    assert!(nodes[0]["error"].is_null());
    assert_eq!(nodes[0]["build"], djbod_client::BUILD);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn object_round_trip_through_http() {
    let test = start_node(4, 3, 1).await;
    let body = pattern_bytes(3 * 3 * 64 * 1024 + 4321, 1);
    // A key with a slash and a space, to check the path is decoded.
    let key = "photos/2026/my cat.jpg";
    let encoded = "photos/2026/my%20cat.jpg";

    let (status, json) = put_object(&test, encoded, &body, Some("image/jpeg")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["key"], key);
    let version = json["version"].as_str().unwrap().to_string();

    let (status, record) = get_json(&test, &format!("/api/objects/{encoded}")).await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(record["key"], key);
    assert_eq!(record["version"], version);
    assert_eq!(record["size"], body.len());
    assert_eq!(record["content_type"], "image/jpeg");
    assert_eq!(record["shards"].as_array().unwrap().len(), 4);

    let (status, headers, fetched) = call(
        &test,
        Request::get(format!("/api/download/{encoded}"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"my_cat.jpg\""
    );
    assert_eq!(headers["x-djbod-version"], version);
    assert_eq!(fetched.as_ref(), body.as_slice());

    let (status, json) = get_json(&test, "/api/objects?prefix=photos/").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["keys"].as_array().unwrap().len(), 1);
    assert_eq!(json["keys"][0]["key"], key);
    assert_eq!(json["truncated"], false);
    let (_, json) = get_json(&test, "/api/objects?prefix=other/").await;
    assert_eq!(json["keys"].as_array().unwrap().len(), 0);

    let (status, report) = post_json(
        &test,
        &format!("/api/repair/{encoded}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["key"], key);
    assert!(report["shards"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["condition"] == "intact" && s["rewritten"] == false));

    let (status, _, out) = call(
        &test,
        Request::delete(format!("/api/objects/{encoded}"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&out));

    let (status, json) = get_json(&test, &format!("/api/objects/{encoded}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["key"], key);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_rebuilds_a_shard_damaged_on_disk() {
    let test = start_node(4, 3, 1).await;
    let body = pattern_bytes(3 * 2 * 64 * 1024 + 99, 7);
    let (status, json) = put_object(&test, "a/b", &body, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // Flip a byte inside shard 0, a data shard: a plain read touches
    // only the k data shards, so damage to the parity shard would go
    // unnoticed by the download and the test would prove nothing.
    let mut damaged = None;
    for dir in &test.dirs {
        for entry in walkdir(dir.path()) {
            if entry.to_string_lossy().ends_with(".0.shard") {
                let mut bytes = std::fs::read(&entry).unwrap();
                bytes[4096 + 10] ^= 0xff;
                std::fs::write(&entry, bytes).unwrap();
                damaged = Some(entry);
                break;
            }
        }
        if damaged.is_some() {
            break;
        }
    }
    assert!(damaged.is_some(), "no shard file written");

    // The headers go out before the damage is met, so the response
    // carries the full length but its body is cut short with an error
    // rather than delivering wrong bytes.
    let response = app(&test)
        .oneshot(
            Request::get("/api/download/a/b")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        body.len().to_string()
    );
    let outcome = response.into_body().collect().await;
    match outcome {
        Err(e) => assert!(e.to_string().contains("BlockChecksumMismatch"), "{e}"),
        Ok(collected) => assert!(
            collected.to_bytes().len() < body.len(),
            "a damaged read must not deliver the whole body"
        ),
    }

    let (status, report) = post_json(&test, "/api/repair/a/b", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let rewritten = report["shards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["rewritten"] == true)
        .count();
    assert_eq!(rewritten, 1, "{report}");

    let (status, _, fetched) = call(
        &test,
        Request::get("/api/download/a/b")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched.as_ref(), body.as_slice());
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_and_scrub_stream_events_as_json_lines() {
    let test = start_node(5, 3, 1).await;
    let body = pattern_bytes(3 * 64 * 1024, 3);
    let (status, json) = put_object(&test, "x/one", &body, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (_, record) = get_json(&test, "/api/objects/x/one").await;
    let device = record["shards"][0]["device"].as_str().unwrap().to_string();

    // Removing is refused through the state endpoint; draining is not.
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{device}/state"),
        serde_json::json!({ "state": "removed" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{device}/state"),
        serde_json::json!({ "state": "draining" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    let (_, status_json) = get_json(&test, "/api/status").await;
    let draining: Vec<&serde_json::Value> = status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["state"] == "draining")
        .collect();
    assert_eq!(draining.len(), 1);
    assert_eq!(draining[0]["device"], device);

    let (status, headers, out) = call(
        &test,
        Request::post(format!("/api/devices/{device}/drain"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert_eq!(lines[0]["event"], "estimate", "{lines:?}");
    assert_eq!(lines[0]["versions"], 1);
    assert_eq!(lines[1]["event"], "moved", "{lines:?}");
    assert_eq!(lines[1]["key"], "x/one");
    assert_eq!(lines.last().unwrap()["event"], "end", "{lines:?}");
    assert!(lines.last().unwrap()["error"].is_null(), "{lines:?}");

    let (_, record) = get_json(&test, "/api/objects/x/one").await;
    assert!(record["shards"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["device"] != device));

    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{device}/remove"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);

    let (status, headers, out) = call(
        &test,
        Request::post("/api/scrub")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"repair": false}"#))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    // The removed device is still open on the node until its path leaves
    // the node's configuration, so the node still scrubs it.
    let summaries = lines
        .iter()
        .filter(|l| l["event"] == "node_summary")
        .count();
    assert_eq!(summaries, 5, "one per device the node has open: {lines:?}");
    assert!(
        lines
            .iter()
            .all(|l| l["event"] != "node_finding" && l["event"] != "cluster_finding"),
        "{lines:?}"
    );
    assert_eq!(lines.last().unwrap()["event"], "end");
    assert!(lines.last().unwrap()["error"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_shard_scheme_and_limits() {
    let test = start_node(5, 3, 1).await;
    let body = pattern_bytes(1000, 5);
    put_object(&test, "m/k", &body, None).await;
    let (_, record) = get_json(&test, "/api/objects/m/k").await;
    let held: Vec<String> = record["shards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["device"].as_str().unwrap().to_string())
        .collect();
    let (_, status_json) = get_json(&test, "/api/status").await;
    let spare = status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["device"].as_str().unwrap().to_string())
        .find(|d| !held.contains(d))
        .expect("a fifth device");

    let (status, json) = post_json(
        &test,
        "/api/move-shard/m/k",
        serde_json::json!({ "shard_index": 2, "to": spare }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["source"], held[2]);
    assert_eq!(json["rebuilt"], false);
    assert_eq!(json["record"]["revision"], 1);
    assert_eq!(json["record"]["shards"][2]["device"], spare);

    let (status, json) = post_json(
        &test,
        "/api/cluster/scheme",
        serde_json::json!({ "k": 2, "m": 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    assert_eq!(json["k"], 2);
    let (status, json) = post_json(
        &test,
        "/api/cluster/scheme",
        serde_json::json!({ "k": 4, "m": 2 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "fewer active devices than k+m: {json}"
    );
    assert_eq!(json["error"]["code"], "too_few_active_devices");

    let (status, json) = post_json(
        &test,
        "/api/cluster/limits",
        serde_json::json!({ "max_object_bytes": 4096 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["max_object_bytes"], 4096);
    let (status, json) = put_object(&test, "too/big", &pattern_bytes(5000, 9), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"]["code"], "object_too_large");
    let (status, json) = post_json(&test, "/api/cluster/limits", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

    let (status, json) = post_json(&test, "/api/cluster/sync", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["already_current"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_requests_are_reported_as_such() {
    let test = start_node(4, 3, 1).await;
    let (status, _, out) = call(
        &test,
        Request::put("/api/objects/no/length")
            .body(Body::from("abc"))
            .expect("request"),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"]["code"], "bad_request");

    // A device that is not a UUID is looked up as a label; an unknown one
    // is refused with a message naming it (its status is decided by the
    // membership error mapping, not here).
    let (status, json) = post_json(
        &test,
        "/api/devices/not-a-uuid/remove",
        serde_json::json!({}),
    )
    .await;
    assert!(!status.is_success(), "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not-a-uuid"),
        "{json}"
    );

    let (status, json) =
        post_json(&test, "/api/nodes/not-a-uuid/remove", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

    // The only node cannot be removed.
    let (_, cluster) = get_json(&test, "/api/cluster").await;
    let node = cluster["document"]["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, json) = post_json(
        &test,
        &format!("/api/nodes/{node}/remove"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "last_node");

    // Something that does not exist is 404.
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{}/remove", Uuid::new_v4()),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["error"]["code"], "unknown_device");
    // A refusal because of the store's state is 409: an active device
    // cannot be removed.
    let (_, status_json) = get_json(&test, "/api/status").await;
    let device = status_json["devices"][0]["device"].as_str().unwrap();
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{device}/remove"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "device_active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_is_down_is_reported_not_crashed() {
    let target = Target {
        node: "127.0.0.1:1".parse().unwrap(),
        cluster: Uuid::new_v4(),
        connector: Connector::plain(),
    };
    let response = router(target)
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "node_unreachable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_check_judges_room_as_the_coordinator_would() {
    let test = start_node(4, 3, 1).await;
    let (status, json) = get_json(&test, "/api/upload-check?size=1000").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fits"], true);
    assert_eq!(json["too_large"], false);
    assert_eq!(json["required_devices"], 4);
    assert_eq!(json["active_devices"], 4);
    assert_eq!(json["devices_with_room"], 4);
    let expected = djbod_core::shardfile::shard_file_length(
        djbod_core::erasure::Scheme::new(3, 1).unwrap(),
        64 * 1024,
        1000,
    )
    .unwrap();
    assert_eq!(json["shard_bytes"], expected);

    // Larger than the maximum object size (1 TiB by default) and than any
    // device: refused on both counts, before a byte is sent.
    let (status, json) = get_json(&test, &format!("/api/upload-check?size={}", 1u64 << 41)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fits"], false);
    assert_eq!(json["too_large"], true);
    assert_eq!(json["devices_with_room"], 0);

    // Under the size limit but over the room: the room is the reason.
    let (_, json) = post_json(
        &test,
        "/api/cluster/limits",
        serde_json::json!({ "max_object_bytes": 1u64 << 50 }),
    )
    .await;
    assert_eq!(json["changed"], true, "{json}");
    let (_, json) = get_json(&test, &format!("/api/upload-check?size={}", 1u64 << 45)).await;
    assert_eq!(json["fits"], false, "{json}");
    assert_eq!(json["too_large"], false);
    assert_eq!(json["devices_with_room"], 0);
    assert!(json["room_bytes"].as_u64().unwrap() > 0);

    // A zero-length object needs no room.
    let (_, json) = get_json(&test, "/api/upload-check?size=0").await;
    assert_eq!(json["fits"], true, "{json}");
    assert_eq!(json["shard_bytes"], 0);
}

/// Over a real socket, as a browser would do it: the node refuses the
/// write at once, but the client keeps sending the body. The server must
/// read the whole body before answering, or the client sees the
/// connection closed mid-send and never learns why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_during_upload_is_delivered_after_the_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let test = start_node(4, 3, 1).await;
    let (_, json) = post_json(
        &test,
        "/api/cluster/limits",
        serde_json::json!({ "max_object_bytes": 4096 }),
    )
    .await;
    assert_eq!(json["changed"], true, "{json}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let router = app(&test);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let size: usize = 64 * 1024 * 1024;
    let mut socket = tokio::net::TcpStream::connect(http).await.unwrap();
    socket
        .write_all(
            format!(
                "PUT /api/objects/too/big HTTP/1.1\r\nHost: {http}\r\nContent-Length: {size}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    // Every write must succeed: a server that closed early would reset
    // the connection part way through.
    let chunk = vec![0xabu8; 64 * 1024];
    let mut sent = 0;
    while sent < size {
        socket
            .write_all(&chunk)
            .await
            .expect("the server kept reading");
        sent += chunk.len();
    }
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 400 "), "{text}");
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let json: serde_json::Value = serde_json::from_str(body.trim()).expect("json body");
    assert_eq!(json["error"]["code"], "object_too_large", "{json}");
}

/// Run a verify and return its JSON lines.
async fn verify_lines(test: &TestNode, key: &str) -> (StatusCode, Vec<serde_json::Value>) {
    let (status, headers, body) = call(
        test,
        Request::post(format!("/api/verify/{key}"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    if status == StatusCode::OK {
        assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
    }
    let lines = String::from_utf8_lossy(&body)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    (status, lines)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verify_names_the_damage_a_download_would_meet() {
    let test = start_node(4, 3, 1).await;
    let body = pattern_bytes(3 * 2 * 64 * 1024 + 7, 11);
    put_object(&test, "v/file", &body, None).await;

    let (status, lines) = verify_lines(&test, "v/file").await;
    assert_eq!(status, StatusCode::OK, "{lines:?}");
    assert_eq!(lines[0]["event"], "start");
    assert_eq!(lines[0]["size"], body.len());
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "done", "{lines:?}");
    assert_eq!(done["verified"], true);
    assert_eq!(done["bytes"], body.len());
    // The last progress line reports the whole object.
    let progress: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["event"] == "progress").collect();
    assert_eq!(progress.last().unwrap()["bytes"], body.len(), "{lines:?}");

    // Damage shard 0, a data shard, inside its data.
    let mut damaged = None;
    for dir in &test.dirs {
        for entry in walkdir(dir.path()) {
            if entry.to_string_lossy().ends_with(".0.shard") {
                let mut bytes = std::fs::read(&entry).unwrap();
                bytes[4096 + 10] ^= 0xff;
                std::fs::write(&entry, bytes).unwrap();
                damaged = Some(entry);
            }
        }
    }
    assert!(damaged.is_some());

    let (status, lines) = verify_lines(&test, "v/file").await;
    assert_eq!(status, StatusCode::OK, "{lines:?}");
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "done", "{lines:?}");
    assert_eq!(done["verified"], false);
    assert_eq!(done["error"]["code"], "block_checksum_mismatch");
    assert_eq!(done["error"]["shard_index"], 0);
    assert_eq!(done["error"]["stripe"], 0);
    assert!(done["error"]["device"].is_string());

    let (status, report) = post_json(&test, "/api/repair/v/file", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let (_, lines) = verify_lines(&test, "v/file").await;
    assert_eq!(lines.last().unwrap()["verified"], true, "{lines:?}");

    let (status, lines) = verify_lines(&test, "v/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{lines:?}");
    assert_eq!(lines[0]["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_labels_are_set_shown_and_cleared() {
    let test = start_node(4, 3, 1).await;
    let (_, status_json) = get_json(&test, "/api/status").await;
    let devices: Vec<String> = status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["device"].as_str().unwrap().to_string())
        .collect();
    assert!(status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d.get("label").is_none()));

    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{}/label", devices[0]),
        serde_json::json!({ "label": "nas1-bay0" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    assert_eq!(json["label"], "nas1-bay0");

    let (_, status_json) = get_json(&test, "/api/status").await;
    let labelled: Vec<&serde_json::Value> = status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["label"] == "nas1-bay0")
        .collect();
    assert_eq!(labelled.len(), 1);
    assert_eq!(labelled[0]["device"], devices[0]);
    let (_, cluster) = get_json(&test, "/api/cluster").await;
    assert!(cluster["document"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["id"] == devices[0] && d["label"] == "nas1-bay0"));

    // The same label again is a no-op; on another device it is refused,
    // as is a label with a space.
    let (_, json) = post_json(
        &test,
        &format!("/api/devices/{}/label", devices[0]),
        serde_json::json!({ "label": "nas1-bay0" }),
    )
    .await;
    assert_eq!(json["changed"], false, "{json}");
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{}/label", devices[1]),
        serde_json::json!({ "label": "nas1-bay0" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "invalid_document");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nas1-bay0"),
        "{json}"
    );
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{}/label", devices[1]),
        serde_json::json!({ "label": "has space" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");

    // Wherever a device is named in a path, its label works too.
    let (status, json) = post_json(
        &test,
        "/api/devices/nas1-bay0/state",
        serde_json::json!({ "state": "draining" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["device"], devices[0]);
    assert_eq!(json["changed"], true);
    let (status, json) = post_json(
        &test,
        "/api/devices/nas1-bay0/state",
        serde_json::json!({ "state": "active" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = post_json(
        &test,
        "/api/devices/no-such-label/state",
        serde_json::json!({ "state": "draining" }),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-label"),
        "{json}"
    );

    // Null, or an empty string, clears it.
    let (status, json) = post_json(
        &test,
        &format!("/api/devices/{}/label", devices[0]),
        serde_json::json!({ "label": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    assert!(json["label"].is_null());
    let (_, status_json) = get_json(&test, "/api/status").await;
    assert!(status_json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d.get("label").is_none()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_the_node_stopped_is_remembered_until_something_succeeds() {
    let test = start_node(4, 3, 1).await;
    let body = pattern_bytes(3 * 2 * 64 * 1024 + 5, 13);
    put_object(&test, "f/one", &body, None).await;
    // The same router instance throughout, since the memory is per server.
    let router = app(&test);
    let get = |path: &str| {
        let router = router.clone();
        let path = path.to_string();
        async move {
            let response = router
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await;
            (status, body)
        }
    };
    let failures = || {
        let router = router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::get("/api/read-failures")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["failures"].as_array().unwrap().clone()
        }
    };

    let (status, collected) = get("/api/download/f/one").await;
    assert_eq!(status, StatusCode::OK);
    assert!(collected.is_ok());
    assert!(failures().await.is_empty());

    for dir in &test.dirs {
        for entry in walkdir(dir.path()) {
            if entry.to_string_lossy().ends_with(".0.shard") {
                let mut bytes = std::fs::read(&entry).unwrap();
                bytes[4096 + 10] ^= 0xff;
                std::fs::write(&entry, bytes).unwrap();
            }
        }
    }

    let (status, collected) = get("/api/download/f/one").await;
    assert_eq!(status, StatusCode::OK);
    assert!(collected.is_err(), "the damaged download must be cut short");
    let listed = failures().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["key"], "f/one");
    assert_eq!(listed[0]["operation"], "download");
    assert_eq!(listed[0]["error"]["code"], "block_checksum_mismatch");
    assert_eq!(listed[0]["error"]["shard_index"], 0);
    assert!(listed[0]["at"].as_u64().unwrap() > 1_700_000_000);

    // A verify that fails refreshes the note with its own operation.
    let response = router
        .clone()
        .oneshot(
            Request::post("/api/verify/f/one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = response.into_body().collect().await.unwrap();
    let listed = failures().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["operation"], "verify");

    // A successful repair clears it.
    let response = router
        .clone()
        .oneshot(
            Request::post("/api/repair/f/one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(failures().await.is_empty());

    // A clean download after the repair leaves nothing behind either.
    let (status, collected) = get("/api/download/f/one").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(collected.unwrap().to_bytes().as_ref(), body.as_slice());
    assert!(failures().await.is_empty());
}

/// A browser made to send a request from another site says so in
/// `Sec-Fetch-Site`, or in `Origin`; the server refuses such a request
/// when it would change anything, and refuses any request for a host name
/// it was not told it answers to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_from_another_site_or_host_are_refused() {
    let test = start_node(4, 3, 1).await;
    let send = |method: &str, path: &str, headers: Vec<(&str, &str)>| {
        let router = app(&test);
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        async move {
            let response = router
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            (status, json)
        }
    };

    // The page's own requests, and a non-browser client, pass.
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![
            ("sec-fetch-site", "same-origin"),
            ("host", "127.0.0.1:5264"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![("sec-fetch-site", "none"), ("host", "192.168.1.10:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![("host", "localhost:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![("host", "[::1]:5264"), ("origin", "http://[::1]:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send("POST", "/api/cluster/sync", vec![]).await;
    assert_eq!(status, StatusCode::OK, "no Host at all is not a browser");

    // A form or script on another site.
    let (status, json) = send(
        "POST",
        "/api/cluster/sync",
        vec![("sec-fetch-site", "cross-site"), ("host", "127.0.0.1:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["error"]["code"], "cross_site");
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![("sec-fetch-site", "same-site"), ("host", "127.0.0.1:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        "POST",
        "/api/cluster/sync",
        vec![
            ("host", "127.0.0.1:5264"),
            ("origin", "http://evil.example"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an old browser says only Origin"
    );
    let (status, _) = send(
        "DELETE",
        "/api/objects/x",
        vec![("sec-fetch-site", "cross-site"), ("host", "127.0.0.1:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Reads from another site are allowed through; the browser's own
    // same-origin policy keeps their bodies from the other page.
    let (status, _) = send(
        "GET",
        "/api/status",
        vec![("sec-fetch-site", "cross-site"), ("host", "127.0.0.1:5264")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A DNS name pointed here by someone else looks same-origin to the
    // browser, so the Host check is what stops it.
    let (status, json) = send(
        "GET",
        "/api/status",
        vec![
            ("sec-fetch-site", "same-origin"),
            ("host", "evil.example:5264"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["error"]["code"], "unknown_host");
    let (status, _) = send("GET", "/", vec![("host", "evil.example")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the page itself too");

    // A name the operator declared is fine.
    let named = router_for_hosts(
        Target {
            node: test.addr,
            cluster: test.node.cluster_id(),
            connector: Connector::plain(),
        },
        vec!["Nas.Example".to_string()],
    );
    let response = named
        .oneshot(
            Request::get("/api/status")
                .header("host", "nas.example:5264")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_labels_are_set_shown_and_cleared() {
    let test = start_node(4, 3, 1).await;
    let (_, cluster) = get_json(&test, "/api/cluster").await;
    let node = cluster["document"]["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cluster["document"]["nodes"][0].get("label").is_none());

    let (status, json) = post_json(
        &test,
        &format!("/api/nodes/{node}/label"),
        serde_json::json!({ "label": "nas1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    let (_, cluster) = get_json(&test, "/api/cluster").await;
    assert_eq!(cluster["document"]["nodes"][0]["label"], "nas1");

    let (status, json) = post_json(
        &test,
        &format!("/api/nodes/{node}/label"),
        serde_json::json!({ "label": "has space" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");

    let (status, json) = post_json(
        &test,
        &format!("/api/nodes/{node}/label"),
        serde_json::json!({ "label": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["changed"], true);
    let (_, cluster) = get_json(&test, "/api/cluster").await;
    assert!(cluster["document"]["nodes"][0].get("label").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_brand_assets_are_served_for_the_tab_and_the_header() {
    let test = start_node(4, 3, 1).await;
    let build = djbod_client::BUILD;
    for (name, kind, magic) in [
        ("lockup.svg", "image/svg+xml", &b"<?xml"[..]),
        ("lockup-dark.svg", "image/svg+xml", &b"<?xml"[..]),
        ("favicon.svg", "image/svg+xml", &b"<"[..]),
        ("favicon-32.png", "image/png", &b"\x89PNG"[..]),
        ("appicon-256.png", "image/png", &b"\x89PNG"[..]),
    ] {
        let path = format!("/brand/{build}/{name}");
        let (status, headers, body) = call(
            &test,
            Request::get(&path).body(Body::empty()).expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers[header::CONTENT_TYPE], kind, "{path}");
        assert!(body.starts_with(magic), "{path} has the wrong content");
        assert!(headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));
    }
    // The page names the assets under this build's id, so a browser that
    // cached an older build's logo fetches the new one.
    let (_, _, page) = call(
        &test,
        Request::get("/").body(Body::empty()).expect("request"),
    )
    .await;
    let text = String::from_utf8_lossy(&page);
    assert!(
        text.contains(&format!(r#"href="/brand/{build}/favicon.svg""#)),
        "{build}"
    );
    assert!(text.contains(&format!(r#"src="/brand/{build}/lockup.svg""#)));
    assert!(!text.contains(r#""/brand/lockup.svg""#));
}
