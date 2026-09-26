//! `djbod-ui`: the administration web UI of SPEC 20.3.
//!
//! One process serves a single page and a small JSON API on a local HTTP
//! port. Every API call is translated into the native protocol and sent
//! to one node of the cluster, exactly as the `djbod` command-line client
//! would send it; the UI holds no state of its own and every action it
//! offers is also a `djbod` command (20.3.1).
//!
//! The API, all under `/api`:
//!
//! | method and path                      | native operation                    |
//! |--------------------------------------|-------------------------------------|
//! | `GET  /status`                       | `Status`                            |
//! | `GET  /cluster`                      | document plus every node's version and build |
//! | `POST /cluster/sync`                 | `djbod cluster sync`                |
//! | `POST /cluster/scheme`               | `djbod cluster set-scheme`          |
//! | `POST /cluster/limits`               | `djbod cluster set-limits`          |
//! | `GET  /objects?prefix&start_after&limit` | `ListKeys`                      |
//! | `GET  /objects/{key}`                | `HeadObject`                        |
//! | `GET  /read-failures`                | reads this server relayed that the node stopped for damage |
//! | `PUT  /objects/{key}`                | `PutObject`, body streamed through  |
//! | `GET  /upload-check?size=N`          | `Status` and the document: would a write of N bytes fit? |
//! | `DELETE /objects/{key}`              | `DeleteObject`                      |
//! | `GET  /download/{key}`               | `GetObject`, body streamed through  |
//! | `POST /repair/{key}`                 | `RepairObject`                      |
//! | `POST /verify/{key}`                 | `GetObject`, body read and discarded; progress and the verdict as NDJSON |
//! | `POST /move-shard/{key}`             | `MoveShard`                         |
//! | `POST /scrub`                        | `Scrub`, events streamed as NDJSON  |
//! | `POST /devices/{id}/state`           | `djbod cluster set-state`           |
//! | `POST /devices/{id}/label`           | `djbod cluster set-device-label`           |
//! | (a device `{id}` is a UUID or a label) |                                   |
//! | `POST /devices/{id}/drain`           | `Drain`, events streamed as NDJSON  |
//! | `POST /devices/{id}/remove`          | `djbod cluster remove-device`       |
//! | `POST /nodes/{id}/remove`            | `djbod cluster remove-node`         |
//! | `POST /nodes/{id}/label`             | `djbod cluster set-node-label`      |
//!
//! Errors are JSON: `{"error": {"code", "message", ...}}`, where the
//! fields are those of the node's `ErrorDetail` (SPEC 16.2) when the
//! node refused, so the administrator sees the same identifying detail
//! the command line prints. The status says whose fault it is: 502 when
//! a node could not be reached, 404 for something that does not exist,
//! 409 when the store refused a well-formed request because of its
//! state, 400 for a malformed request (see `membership_status`).
//!
//! The HTTP side has no authentication of its own, so the server binds
//! to localhost by default, and it refuses two things a browser could
//! otherwise be made to do from another site: a request that changes
//! anything (any method but GET) whose `Sec-Fetch-Site` or `Origin`
//! says it came from another origin, and any request whose `Host` is a
//! name this server was not told it answers to, which is how a hostile
//! DNS name pointed at this address would look (see `same_origin_only`). Towards the cluster it connects as the
//! `djbod` client does: plain, or TLS with the same `--tls-ca`,
//! `--tls-cert`, and `--tls-key` settings (19.1.6.2).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request as HttpRequest, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::io::StreamReader;
use uuid::Uuid;

use djbod_client::admin::{self, AdminError};
use djbod_client::connection::{Connection, ConnectionError, StreamItem, DEFAULT_BODY_CHUNK};
use djbod_client::transport::Connector;
use djbod_client::wire::WireError;
use djbod_core::checksum::checksum_block;
use djbod_core::cluster::{DeviceState, NodeId};
use djbod_core::erasure::Scheme;
use djbod_core::record::DeviceId;
use djbod_proto::message::{
    ErrorCode, ErrorDetail, ListQuery, MissingRecordCopy, Reconstruction, RecordCopyFault, Request,
    Response as Reply,
};

/// The page, embedded so the binary is self-contained.
pub const PAGE: &str = include_str!("../ui.html");
/// The brand assets the page uses, from docs/brand (see its README). The
/// parity slabs are yellow throughout: the header's lockup is the
/// status-light kit, whose slabs glow like lit indicators, and the tab
/// and app icons are the plain yellow kit, which reads better small.
/// The one-line unhyphenated lockup for the header in its light and dark forms,
/// the 2x2 favicon reduction for the tab with a PNG fallback, and the
/// app icon for home screens.
pub const LOCKUP: &[u8] =
    include_bytes!("../../../docs/brand/status-light/djbod-lockup-inline-nohyphen.svg");
pub const LOCKUP_DARK: &[u8] =
    include_bytes!("../../../docs/brand/status-light/djbod-lockup-inline-nohyphen-onDark.svg");
pub const FAVICON_SVG: &[u8] = include_bytes!("../../../docs/brand/yellow/djbod-favicon.svg");
pub const FAVICON_PNG: &[u8] =
    include_bytes!("../../../docs/brand/yellow/png/djbod-favicon-32.png");
pub const APP_ICON: &[u8] = include_bytes!("../../../docs/brand/yellow/png/djbod-appicon-256.png");

/// A read of an object that the node stopped because of damage, kept so
/// the page can say why a download failed after the browser has reported
/// only that it did. The UI server is the one relaying the read, so it
/// is the only party outside the node that sees the node's error. Held
/// in memory, one per key, most recent wins; cleared when a read, verify,
/// or repair of the key succeeds or the key is deleted. This is a
/// stopgap for a store that forgets what it found (see
/// docs/proposals/damage-marks.md): it lives in one process, dies with
/// it, knows only about reads that went through it, and is found by the
/// page by asking, which for a slow download may be after the page has
/// stopped asking (see `watchForReadFailure` in ui.html).
#[derive(Debug, Clone, Serialize)]
pub struct ReadFailure {
    pub key: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// Which operation met the damage: `download` or `verify`.
    pub operation: &'static str,
    /// Bytes delivered before the node stopped.
    pub bytes: u64,
    pub error: ErrorDetail,
}

/// Everything the handlers share: which cluster, the recent read
/// failures, and the host names this server answers to.
pub struct App {
    pub target: Target,
    /// Which node answered last, and the addresses learnt from the
    /// cluster document, so a node leaving does not take the page down.
    peers: std::sync::Mutex<Peers>,
    failures: std::sync::Mutex<std::collections::HashMap<String, ReadFailure>>,
    /// Host names, without port, accepted in `Host` besides IP literals
    /// and `localhost`.
    hosts: Vec<String>,
}

/// At most this many keys are remembered; the oldest go first.
const MAX_FAILURES: usize = 1000;

/// What the server knows about where the cluster's nodes are, beyond
/// the addresses it was started with.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Peers {
    /// The address that answered last; tried first next time, so a
    /// working node is not abandoned for a dead one earlier in the list.
    preferred: Option<SocketAddr>,
    /// Every node's addresses as the cluster document last listed them.
    learned: Vec<SocketAddr>,
}

/// The order in which to try addresses: the one that answered last, the
/// configured ones in their order, then the ones learnt from the cluster
/// document; each address once.
fn candidates(
    preferred: Option<SocketAddr>,
    configured: &[SocketAddr],
    learned: &[SocketAddr],
) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::with_capacity(1 + configured.len() + learned.len());
    for address in preferred.iter().chain(configured).chain(learned) {
        if !out.contains(address) {
            out.push(*address);
        }
    }
    out
}

impl App {
    /// Open a connection to the first node that answers, in the order of
    /// `candidates`, and remember it as preferred. Fails with every
    /// address tried and why when none answers.
    async fn connect_any(&self) -> ApiResult<(SocketAddr, Connection)> {
        let (preferred, learned) = {
            let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
            (peers.preferred, peers.learned.clone())
        };
        let mut attempts = Vec::new();
        for address in candidates(preferred, &self.target.nodes, &learned) {
            match Connection::connect_with(
                &self.target.connector,
                address,
                Connection::client_hello(self.target.cluster),
            )
            .await
            {
                Ok(connection) => {
                    self.peers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .preferred = Some(address);
                    return Ok((address, connection));
                }
                Err(e) => attempts.push(Attempt::new(address, e)),
            }
        }
        Err(ApiError::Unreachable { attempts })
    }

    /// The address of a node that answers, for the membership procedures,
    /// which take one peer and open their own connections to it.
    async fn peer(&self) -> ApiResult<SocketAddr> {
        Ok(self.connect_any().await?.0)
    }

    /// Remember every address the cluster document lists, so that the
    /// nodes this server was started with are not the only ones it can
    /// reach. Strings that are not `ip:port` are skipped; a node with such
    /// an address is reported by the cluster view anyway.
    fn learn(&self, document: &djbod_core::cluster::ClusterDocument) {
        let learned: Vec<SocketAddr> = document
            .nodes
            .iter()
            .flat_map(|node| node.addresses.iter())
            .filter_map(|text| text.parse().ok())
            .collect();
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).learned = learned;
    }

    fn record_failure(&self, key: &str, operation: &'static str, bytes: u64, error: ErrorDetail) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if failures.len() >= MAX_FAILURES && !failures.contains_key(key) {
            if let Some(oldest) = failures
                .values()
                .min_by_key(|f| f.at)
                .map(|f| f.key.clone())
            {
                failures.remove(&oldest);
            }
        }
        failures.insert(
            key.to_string(),
            ReadFailure {
                key: key.to_string(),
                at,
                operation,
                bytes,
                error,
            },
        );
    }

    /// The note a degraded read leaves (SPEC 11.4, 9.4.4): blocks it
    /// reconstructed from parity, record copies it went without, or
    /// both. The bytes were right, the cluster is not whole, and repair
    /// fixes what is damage; a device that is out mends nothing.
    fn degraded_detail(
        reconstructed: &[Reconstruction],
        missing_records: &[MissingRecordCopy],
    ) -> ErrorDetail {
        let mut notes = Vec::new();
        if !reconstructed.is_empty() {
            notes.push(format!(
                "{} reconstructed from parity; the data was correct, the damage on disk is not repaired",
                djbod_core::text::counted(reconstructed.len(), "block", "blocks")
            ));
        }
        for copy in missing_records {
            notes.push(match &copy.fault {
                RecordCopyFault::Missing => format!("record copy missing on device {}", copy.device),
                RecordCopyFault::Stale { revision } => format!(
                    "record copy on device {} is at revision {revision}, an interrupted re-placement",
                    copy.device
                ),
                RecordCopyFault::Unavailable { reason } => format!(
                    "record copy on device {} could not be read: {reason}",
                    copy.device
                ),
            });
        }
        match reconstructed.first() {
            Some(first) => ErrorDetail {
                device: Some(first.device),
                shard_index: Some(first.shard_index),
                stripe: Some(first.first_stripe),
                ..ErrorDetail::new(ErrorCode::BlockChecksumMismatch, notes.join("; "))
            },
            None => ErrorDetail {
                device: missing_records.first().map(|m| m.device),
                ..ErrorDetail::new(ErrorCode::RecordsInconsistent, notes.join("; "))
            },
        }
    }

    fn clear_failure(&self, key: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    fn failures(&self) -> Vec<ReadFailure> {
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<ReadFailure> = failures.values().cloned().collect();
        all.sort_by(|a, b| b.at.cmp(&a.at).then(a.key.cmp(&b.key)));
        all
    }
}

/// Which cluster the UI administers and how to reach it: any node's
/// address, the cluster id, and the connector (plain or TLS), the same
/// values the command-line client needs.
#[derive(Clone)]
pub struct Target {
    /// The nodes to connect through, tried in order; the addresses of
    /// the other nodes in the cluster document are tried after them once
    /// the document has been fetched (`App::learn`). The same list, and
    /// the same meaning, as `djbod --bootstrap-node`.
    pub nodes: Vec<SocketAddr>,
    pub cluster: Uuid,
    pub connector: Connector,
}

/// The whole application: the page at `/` and the API under `/api`,
/// answering to IP-literal and `localhost` hosts only.
pub fn router(target: Target) -> Router {
    router_for_hosts(target, Vec::new())
}

/// As `router`, also answering to the given host names (SPEC 20.3): a
/// request whose `Host` is any other name is refused, because a name an
/// attacker controls can be pointed at this address and then looks, to
/// the browser, like the attacker's own origin.
pub fn router_for_hosts(target: Target, hosts: Vec<String>) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/cluster", get(cluster))
        .route("/cluster/sync", post(cluster_sync))
        .route("/cluster/scheme", post(cluster_scheme))
        .route("/cluster/limits", post(cluster_limits))
        .route("/objects", get(list_objects))
        .route("/read-failures", get(read_failures))
        .route("/upload-check", get(upload_check))
        .route(
            "/objects/{*key}",
            get(head_object).put(put_object).delete(delete_object),
        )
        .route("/download/{*key}", get(download_object))
        .route("/repair/{*key}", post(repair_object))
        .route("/verify/{*key}", post(verify_object))
        .route("/move-shard/{*key}", post(move_shard))
        .route("/scrub", post(scrub))
        .route("/devices/{id}/state", post(set_device_state))
        .route("/devices/{id}/label", post(set_device_label))
        .route("/devices/{id}/drain", post(drain))
        .route("/devices/{id}/remove", post(remove_device))
        .route("/nodes/{id}/remove", post(remove_node))
        .route("/nodes/{id}/label", post(set_node_label))
        // Object bodies are as large as the cluster allows, not as large
        // as axum's default two megabytes.
        .layer(DefaultBodyLimit::disable());
    let app = Arc::new(App {
        target,
        peers: std::sync::Mutex::new(Peers::default()),
        failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        hosts: hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect(),
    });
    Router::new()
        .route("/", get(page))
        .route(
            "/brand/{version}/lockup.svg",
            get(|| async { asset(LOCKUP, "image/svg+xml") }),
        )
        .route(
            "/brand/{version}/lockup-dark.svg",
            get(|| async { asset(LOCKUP_DARK, "image/svg+xml") }),
        )
        .route(
            "/brand/{version}/favicon.svg",
            get(|| async { asset(FAVICON_SVG, "image/svg+xml") }),
        )
        .route(
            "/brand/{version}/favicon-32.png",
            get(|| async { asset(FAVICON_PNG, "image/png") }),
        )
        .route(
            "/brand/{version}/appicon-256.png",
            get(|| async { asset(APP_ICON, "image/png") }),
        )
        .nest("/api", api)
        .layer(middleware::from_fn_with_state(
            app.clone(),
            same_origin_only,
        ))
        .with_state(app)
}

/// Refuse what a browser could be made to send from another site.
///
/// Two checks. First, `Host` must be an IP literal, `localhost`, or a
/// name this server was told it answers to; anything else is a DNS name
/// pointed here by someone else, and is refused whatever the method.
/// Second, a request with any method but GET or HEAD must come from this
/// page: `Sec-Fetch-Site` is `same-origin` or `none` (typed or a
/// bookmark), or, when a client sends no such header, `Origin` is absent
/// (not a browser) or names this host. A form on another site posts
/// with `Sec-Fetch-Site: cross-site` and is refused; the page's own
/// requests pass. This is not authentication (there is none, 19.1.6),
/// only the browser's own word about where a request came from.
async fn same_origin_only(
    State(app): State<Arc<App>>,
    request: HttpRequest,
    next: Next,
) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_name)
        .unwrap_or_default();
    if !app.host_allowed(&host) {
        return refuse(
            "unknown_host",
            format!("this server does not answer to the host name {host:?}; use its address, or start it with --host {host}"),
        );
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        let same_origin = match (site, origin) {
            (Some("same-origin"), _) | (Some("none"), _) => true,
            (Some(_), _) => false,
            (None, None) => true,
            (None, Some(origin)) => {
                let origin_host = origin
                    .split_once("://")
                    .map(|(_, rest)| rest)
                    .unwrap_or(origin);
                host_name(origin_host) == host
            }
        };
        if !same_origin {
            return refuse(
                "cross_site",
                "refused: this request did not come from the page itself",
            );
        }
    }
    next.run(request).await
}

/// The name part of `host[:port]`, lower-cased; an IPv6 literal keeps
/// its brackets.
fn host_name(authority: &str) -> String {
    let authority = authority.trim();
    let without_port = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map(|(h, _)| format!("{h}]"))
            .unwrap_or(authority.to_string())
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or(authority.to_string())
    };
    without_port.to_ascii_lowercase()
}

impl App {
    fn host_allowed(&self, host: &str) -> bool {
        if host.is_empty() {
            // HTTP/1.0, or a client that sent none: not a browser.
            return true;
        }
        if host == "localhost" || host.ends_with(".localhost") {
            return true;
        }
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        if literal.parse::<std::net::IpAddr>().is_ok() {
            return true;
        }
        self.hosts.iter().any(|h| h == host)
    }
}

fn refuse(code: &str, message: impl Into<String>) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

/// The page, with every brand asset path given this build's id as a
/// segment, so a browser may cache the assets indefinitely and still
/// see a new logo the moment a new build serves the page.
static PAGE_FOR_THIS_BUILD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    PAGE.replace("\"/brand/", &format!("\"/brand/{}/", djbod_client::BUILD))
});

async fn page() -> Html<&'static str> {
    Html(PAGE_FOR_THIS_BUILD.as_str())
}

fn asset(bytes: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                // The path carries the build id, so the content at a given
                // path never changes and may be cached for good.
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        bytes,
    )
        .into_response()
}

// ---------------------------------------------------------------- errors

/// What a handler can fail with, and how each is reported.
#[derive(Debug)]
pub enum ApiError {
    /// The node answered with an error; reported with every field.
    Remote(ErrorDetail),
    /// The node could not be reached or talked to.
    Client(Box<ConnectionError>),
    /// No node answered: every address tried and why, in plain words,
    /// with the raw error kept for debugging.
    Unreachable { attempts: Vec<Attempt> },
    /// A document change failed.
    Membership(Box<AdminError>),
    /// The request itself was wrong.
    BadRequest(String),
}

impl From<ConnectionError> for ApiError {
    fn from(e: ConnectionError) -> ApiError {
        match e {
            ConnectionError::Remote(detail) | ConnectionError::StreamFailed(detail) => {
                ApiError::Remote(detail)
            }
            other => ApiError::Client(Box::new(other)),
        }
    }
}

impl From<AdminError> for ApiError {
    fn from(e: AdminError) -> ApiError {
        ApiError::Membership(Box::new(e))
    }
}

impl ApiError {
    fn unexpected(reply: Reply) -> ApiError {
        ApiError::Client(Box::new(ConnectionError::UnexpectedMessage {
            expected: "the operation's response",
            got: format!("{reply:?}"),
        }))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            ApiError::Remote(detail) => {
                let status = match detail.code {
                    ErrorCode::NotFound => StatusCode::NOT_FOUND,
                    ErrorCode::KeyTooLong
                    | ErrorCode::ObjectTooLarge
                    | ErrorCode::MetadataTooLarge
                    | ErrorCode::ProtocolViolation => StatusCode::BAD_REQUEST,
                    // The cluster answered and refused one object because a
                    // node or device it needs is out (9.4.4, 11.4): a
                    // refusal by the store, not a gateway failure, which is
                    // what a 502 means to the page.
                    ErrorCode::NodeUnreachable | ErrorCode::DeviceUnavailable
                        if detail.key.is_some() =>
                    {
                        StatusCode::CONFLICT
                    }
                    _ => StatusCode::BAD_GATEWAY,
                };
                (status, json!({ "error": detail }))
            }
            ApiError::Client(e) => (
                StatusCode::BAD_GATEWAY,
                json!({ "error": { "code": "node_unreachable", "message": e.to_string() } }),
            ),
            ApiError::Unreachable { attempts } => {
                // What the page shows: the address tried and why it
                // failed, in words an operator can act on; with several
                // addresses, each in turn.
                let message = match attempts.as_slice() {
                    [one] => format!("node {} is not reachable: {}", one.address, one.reason),
                    many => format!(
                        "no node is reachable: {}",
                        many.iter()
                            .map(|a| format!("{}: {}", a.address, a.reason))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                };
                let mut error = json!({
                    "code": "node_unreachable",
                    "message": message,
                    "addresses": attempts.iter().map(|a| a.address.to_string()).collect::<Vec<_>>(),
                    // The operating system's own text, for debugging.
                    "detail": attempts
                        .iter()
                        .map(|a| if attempts.len() == 1 { a.detail.clone() } else { format!("{}: {}", a.address, a.detail) })
                        .collect::<Vec<_>>()
                        .join("; "),
                });
                if let [one] = attempts.as_slice() {
                    error["address"] = json!(one.address.to_string());
                }
                (StatusCode::BAD_GATEWAY, json!({ "error": error }))
            }
            ApiError::Membership(e) => {
                let (status, code) = membership_status(&e);
                (
                    status,
                    json!({ "error": { "code": code, "message": e.to_string() } }),
                )
            }
            ApiError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": { "code": "bad_request", "message": message } }),
            ),
        };
        (status, Json(body)).into_response()
    }
}

/// One address that was tried and did not answer.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub address: SocketAddr,
    /// Why, in plain words: "connection refused", "timed out".
    pub reason: String,
    /// The error's own text, for debugging.
    pub detail: String,
}

impl Attempt {
    /// A failed connection attempt to `address`. An I/O error is worded
    /// for the page; a peer that answered and refused keeps its own
    /// description.
    fn new(address: SocketAddr, e: ConnectionError) -> Attempt {
        let (reason, detail) = match &e {
            ConnectionError::Wire(WireError::Io(io)) => (plain_words(io), io.to_string()),
            ConnectionError::Wire(WireError::Closed) => (
                "the connection was closed before the node answered".to_string(),
                e.to_string(),
            ),
            other => (other.to_string(), other.to_string()),
        };
        Attempt {
            address,
            reason,
            detail,
        }
    }
}

/// An I/O error in the words an operator uses, without the operating
/// system's error number: "connection refused" rather than
/// "Connection refused (os error 111)".
fn plain_words(e: &io::Error) -> String {
    use io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => "connection refused".to_string(),
        K::ConnectionReset => "connection reset".to_string(),
        K::ConnectionAborted => "connection aborted".to_string(),
        K::TimedOut => "timed out".to_string(),
        K::HostUnreachable => "host unreachable".to_string(),
        K::NetworkUnreachable => "network unreachable".to_string(),
        K::NetworkDown => "network down".to_string(),
        K::AddrNotAvailable => "address not available".to_string(),
        K::PermissionDenied => "permission denied".to_string(),
        _ => {
            // Whatever the system said, minus its trailing "(os error N)".
            let text = e.to_string();
            let text = match text.rfind(" (os error ") {
                Some(cut) if text.ends_with(')') => &text[..cut],
                _ => &text,
            };
            let mut chars = text.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => "unknown error".to_string(),
            }
        }
    }
}

/// Whose fault a failed document change is, so the page can tell a
/// refusal from an outage: 502 when a node could not be reached or the
/// change only got part way round; 404 when the request named something
/// that does not exist; 409 when the store refused a well-formed request
/// because of its current state, or another change won the race, which
/// a retry settles; 500 for a fault in this server's own configuration.
/// The code is the error's name, for scripts.
fn membership_status(e: &AdminError) -> (StatusCode, &'static str) {
    use AdminError as M;
    match e {
        M::BadAddress { .. } => (StatusCode::BAD_GATEWAY, "bad_address"),
        M::Unreachable { .. } => (StatusCode::BAD_GATEWAY, "unreachable"),
        M::PeerUnreachable { .. } => (StatusCode::BAD_GATEWAY, "peer_unreachable"),
        M::Partial { .. } => (StatusCode::BAD_GATEWAY, "partial"),
        M::UnexpectedResponse { .. } => (StatusCode::BAD_GATEWAY, "unexpected_response"),
        M::TooManyRetries(_) => (StatusCode::BAD_GATEWAY, "too_many_retries"),
        M::WrongCluster { .. } => (StatusCode::BAD_GATEWAY, "wrong_cluster"),
        M::Diverged { .. } => (StatusCode::BAD_GATEWAY, "diverged"),
        M::UnknownDevice(_) => (StatusCode::NOT_FOUND, "unknown_device"),
        M::UnknownDeviceName(_) => (StatusCode::NOT_FOUND, "unknown_device_name"),
        M::UnknownNode(_) => (StatusCode::NOT_FOUND, "unknown_node"),
        M::NodeRemoved(_) => (StatusCode::CONFLICT, "node_removed"),
        M::UnknownNodeName(_) => (StatusCode::NOT_FOUND, "unknown_node_name"),
        M::VersionsDiffer(_) => (StatusCode::CONFLICT, "versions_differ"),
        M::StaleProposal { .. } => (StatusCode::CONFLICT, "stale_proposal"),
        M::Superseded { .. } => (StatusCode::CONFLICT, "superseded"),
        M::StillReferenced { .. } => (StatusCode::CONFLICT, "still_referenced"),
        M::DeviceActive(_) => (StatusCode::CONFLICT, "device_active"),
        M::NodeHasActiveDevices { .. } => (StatusCode::CONFLICT, "node_has_active_devices"),
        M::NodeIsAlive { .. } => (StatusCode::CONFLICT, "node_is_alive"),
        M::LastNode => (StatusCode::CONFLICT, "last_node"),
        M::NodeNotTlsReady { .. } => (StatusCode::CONFLICT, "node_not_tls_ready"),
        M::TlsRequired { .. } => (StatusCode::CONFLICT, "tls_required"),
        M::TooFewActiveDevices { .. } => (StatusCode::CONFLICT, "too_few_active_devices"),
        // A document that fails validation: a bad or duplicate label, a
        // scheme the devices cannot carry, and the like.
        M::Document(_) => (StatusCode::CONFLICT, "invalid_document"),
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

async fn connect(app: &App) -> ApiResult<Connection> {
    Ok(app.connect_any().await?.1)
}

/// The device a path parameter names: a UUID, or a label looked up in
/// the cluster document (SPEC 6.2.5.1), as every `djbod` command that
/// takes a device accepts either. A label costs one document fetch.
async fn device_param(app: &App, name: &str) -> ApiResult<DeviceId> {
    match Uuid::parse_str(name) {
        Ok(uuid) => Ok(DeviceId(uuid)),
        Err(_) => Ok(admin::resolve_device(
            &app.target.connector,
            app.peer().await?,
            app.target.cluster,
            name,
        )
        .await?),
    }
}

fn parse_id(id: &str, what: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(id).map_err(|_| ApiError::BadRequest(format!("{what} id {id:?} is not a UUID")))
}

// ---------------------------------------------------------------- status

async fn status(State(app): State<Arc<App>>) -> ApiResult {
    let (via, mut conn) = app.connect_any().await?;
    match conn.request(Request::Status).await? {
        Reply::Status {
            cluster_id,
            cluster_name,
            document_version,
            coordinator,
            nodes,
            transport,
            devices,
        } => Ok(Json(json!({
            "cluster_id": cluster_id,
            // The cluster's name, if it has one (SPEC 6.2.5.3).
            "cluster_name": cluster_name,
            "document_version": document_version,
            "coordinator": coordinator,
            // The cluster's transport (SPEC 19.1.6.3): plain, tls-optional,
            // or tls; and whether this server's own connection to the node
            // is TLS, which follows from how it was started.
            "transport": transport,
            "ui_to_node_tls": conn.is_tls(),
            // The address this server reached the cluster through for
            // this answer; it moves to another node when one fails.
            "via": via.to_string(),
            // This server's own build, shown in the page header.
            "ui_build": djbod_client::BUILD,
            // Every node asked and its build (SPEC 6.2.6.4).
            "nodes": nodes,
            "devices": devices,
        }))),
        other => Err(ApiError::unexpected(other)),
    }
}

/// The document as the target node holds it, and what every node listed
/// in it answers when asked for its own version (`djbod cluster show`).
async fn cluster(State(app): State<Arc<App>>) -> ApiResult {
    let target = &app.target;
    let document =
        admin::fetch_document(&target.connector, app.peer().await?, target.cluster).await?;
    app.learn(&document);
    let reports = admin::fetch_all(&target.connector, &document).await;
    let mut nodes: Vec<Value> = reports
        .iter()
        .map(|r| {
            json!({
                "node": r.node,
                "state": "active",
                "address": r.address,
                // From the node's Hello; null for a node that was unreachable
                // or runs a build from before builds were sent (SPEC 6.2.6.4).
                "build": r.build,
                "version": r.result.as_ref().ok().map(|d| d.version),
                "error": r.result.as_ref().err(),
            })
        })
        .collect();
    // Removed nodes (SPEC 6.2.2) are tombstones: listed from the document
    // for the record, asked nothing, so the page can show or hide them.
    for n in document
        .nodes
        .iter()
        .filter(|n| n.state == djbod_core::cluster::NodeState::Removed)
    {
        nodes.push(json!({
            "node": n.id,
            "state": "removed",
            "address": n.addresses.first(),
            "build": null,
            "version": null,
            "error": null,
        }));
    }
    Ok(Json(json!({ "document": document, "nodes": nodes })))
}

async fn cluster_sync(State(app): State<Arc<App>>) -> ApiResult {
    let target = &app.target;
    let report = admin::sync(&target.connector, app.peer().await?, target.cluster).await?;
    Ok(Json(json!({
        "highest_version": report.highest_version,
        "updated": report.updated,
        "already_current": report.already_current,
        "unreachable": report.unreachable,
    })))
}

#[derive(Deserialize)]
struct SchemeBody {
    k: u8,
    m: u8,
    block_size: Option<u64>,
}

async fn cluster_scheme(State(app): State<Arc<App>>, Json(body): Json<SchemeBody>) -> ApiResult {
    let target = &app.target;
    let (document, changed) = admin::set_scheme(
        &target.connector,
        app.peer().await?,
        target.cluster,
        body.k,
        body.m,
        body.block_size,
    )
    .await?;
    Ok(Json(json!({
        "k": document.k,
        "m": document.m,
        "block_size": document.block_size,
        "document_version": document.version,
        "changed": changed,
    })))
}

#[derive(Deserialize)]
struct LimitsBody {
    max_key_bytes: Option<u64>,
    max_object_bytes: Option<u64>,
    max_user_metadata_bytes: Option<u64>,
}

async fn cluster_limits(State(app): State<Arc<App>>, Json(body): Json<LimitsBody>) -> ApiResult {
    let target = &app.target;
    if body.max_key_bytes.is_none()
        && body.max_object_bytes.is_none()
        && body.max_user_metadata_bytes.is_none()
    {
        return Err(ApiError::BadRequest("no limit given".to_string()));
    }
    let (document, changed) = admin::set_limits(
        &target.connector,
        app.peer().await?,
        target.cluster,
        body.max_key_bytes,
        body.max_object_bytes,
        body.max_user_metadata_bytes,
    )
    .await?;
    Ok(Json(json!({
        "max_key_bytes": document.max_key_bytes,
        "max_object_bytes": document.max_object_bytes,
        "max_user_metadata_bytes": document.max_user_metadata_bytes,
        "document_version": document.version,
        "changed": changed,
    })))
}

// --------------------------------------------------------------- objects

#[derive(Deserialize)]
struct ListParams {
    prefix: Option<String>,
    start_after: Option<String>,
    limit: Option<u32>,
}

async fn list_objects(State(app): State<Arc<App>>, Query(params): Query<ListParams>) -> ApiResult {
    let mut conn = connect(&app).await?;
    let query = ListQuery {
        prefix: params.prefix.filter(|p| !p.is_empty()),
        start_after: params.start_after.filter(|s| !s.is_empty()),
        limit: params.limit,
    };
    match conn.request(Request::ListKeys(query)).await? {
        Reply::ListKeys {
            keys,
            truncated,
            unread,
            complete,
        } => Ok(Json(
            json!({ "keys": keys, "truncated": truncated, "unread": unread, "complete": complete }),
        )),
        other => Err(ApiError::unexpected(other)),
    }
}

/// Recent read failures this server relayed, most recent first.
async fn read_failures(State(app): State<Arc<App>>) -> ApiResult {
    Ok(Json(json!({ "failures": app.failures() })))
}

async fn head_object(State(app): State<Arc<App>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&app).await?;
    match conn.request(Request::HeadObject { key }).await? {
        Reply::HeadObject {
            record,
            missing_records,
        } => {
            // A copy the lookup went without (SPEC 9.4.4) is noted like a
            // reconstruction, so the page shows it until a repair.
            if !missing_records.is_empty() {
                app.record_failure(
                    &record.key,
                    "head",
                    0,
                    App::degraded_detail(&[], &missing_records),
                );
            }
            let mut value = json!(record);
            value["missing_records"] = json!(missing_records);
            Ok(Json(value))
        }
        other => Err(ApiError::unexpected(other)),
    }
}

/// Store the request body under the key. The size must be known up front
/// (SPEC 10.1), so `Content-Length` is required; browsers send it for a
/// `File` body. The body is streamed to the node a chunk at a time.
async fn put_object(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let size: u64 = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::BadRequest("Content-Length is required".to_string()))?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let mut conn = connect(&app).await?;
    let mut source = StreamReader::new(body.into_data_stream().map_err(io::Error::other));
    let result = conn
        .put_object_from_reader(&key, size, &mut source, DEFAULT_BODY_CHUNK, content_type)
        .await;
    let write = match result {
        Ok(write) => write,
        Err(e) => {
            // The node has refused, but the browser may still be sending
            // the body. A response on a connection whose request body was
            // never read makes the browser report a network failure and
            // drop the response, so the reason would be lost. Read and
            // discard the rest first; the refusal is then delivered.
            let _ = tokio::io::copy(&mut source, &mut tokio::io::sink()).await;
            return Err(e.into());
        }
    };
    Ok(Json(json!({
        "key": key,
        "version": write.version.to_text(),
        // Devices the write went around because their node cannot read
        // them (SPEC 5.6).
        "unavailable": write.unavailable,
    })))
}

#[derive(Deserialize)]
struct UploadCheckParams {
    size: u64,
}

/// Whether a write of `size` bytes would find room, judged as the
/// coordinator judges it (SPEC 10.4): k+m active devices each with at
/// least one shard file's worth of free space within headroom, which is
/// what `Status` reports. The page asks before starting an upload so a
/// file that plainly will not fit is refused before its bytes are sent;
/// the node remains the authority when the upload runs. Devices that
/// share one filesystem report the same free space several times, so
/// the answer is optimistic there.
async fn upload_check(
    State(app): State<Arc<App>>,
    Query(params): Query<UploadCheckParams>,
) -> ApiResult {
    let mut conn = connect(&app).await?;
    let document = match conn.request(Request::GetClusterConfig).await? {
        Reply::GetClusterConfig { document } => document,
        other => return Err(ApiError::unexpected(other)),
    };
    let devices = match conn.request(Request::Status).await? {
        Reply::Status { devices, .. } => devices,
        other => return Err(ApiError::unexpected(other)),
    };
    let internal =
        |message: String| ApiError::Remote(ErrorDetail::new(ErrorCode::Internal, message));
    let scheme = Scheme::new(document.k, document.m)
        .map_err(|e| internal(format!("the cluster document's scheme is invalid: {e}")))?;
    let shard_bytes = if params.size == 0 {
        0
    } else {
        djbod_core::shardfile::shard_file_length(scheme, document.block_size, params.size)
            .ok_or_else(|| internal("could not compute the shard file length".to_string()))?
    };
    let required = scheme.total_shards();
    let mut active_free: Vec<u64> = devices
        .iter()
        .filter(|d| d.state == DeviceState::Active)
        .map(|d| d.free_bytes)
        .collect();
    active_free.sort_unstable_by(|a, b| b.cmp(a));
    let with_room = active_free
        .iter()
        .filter(|free| **free >= shard_bytes)
        .count();
    Ok(Json(json!({
        "size": params.size,
        "fits": with_room >= required && params.size <= document.max_object_bytes,
        "too_large": params.size > document.max_object_bytes,
        "max_object_bytes": document.max_object_bytes,
        "shard_bytes": shard_bytes,
        "required_devices": required,
        "active_devices": active_free.len(),
        "devices_with_room": with_room,
        // Free space on the device that would receive the last shard:
        // the k+m-th emptiest active device, or 0 if there are fewer.
        "room_bytes": active_free.get(required.saturating_sub(1)).copied().unwrap_or(0),
    })))
}

async fn delete_object(State(app): State<Arc<App>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&app).await?;
    match conn
        .request(Request::DeleteObject { key: key.clone() })
        .await?
    {
        Reply::DeleteObject => {
            app.clear_failure(&key);
            Ok(Json(json!({ "key": key, "deleted": true })))
        }
        other => Err(ApiError::unexpected(other)),
    }
}

/// Stream an object's body out as the HTTP response. The record arrives
/// before the body, which gives the headers; each chunk is verified in
/// transit as the client does. A failure mid-body, including the
/// coordinator's whole-object check at the end (SPEC 11.7), cannot be
/// reported after the headers have gone, so the response is cut short of
/// its declared `Content-Length` and the browser reports a failed
/// download rather than presenting a wrong file.
async fn download_object(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    let mut conn = connect(&app).await?;
    let id = conn
        .send_request(Request::GetObject { key: key.clone() })
        .await?;
    let record = match conn.read_response(id).await? {
        Reply::GetObject { record } => record,
        other => return Err(ApiError::unexpected(other)),
    };
    let app_for_stream = app.clone();
    let key_for_stream = key.clone();
    let body = futures_util::stream::unfold(
        (conn, 0u64, 0u64, false),
        move |(mut conn, expected_sequence, delivered, finished)| {
            let app = app_for_stream.clone();
            let key = key_for_stream.clone();
            async move {
                if finished {
                    return None;
                }
                let item = match conn.read_stream_item(id).await {
                    Ok(item) => item,
                    Err(e) => {
                        let detail = match e {
                            ConnectionError::Remote(d) | ConnectionError::StreamFailed(d) => d,
                            other => ErrorDetail::new(ErrorCode::Internal, other.to_string()),
                        };
                        app.record_failure(&key, "download", delivered, detail.clone());
                        return Some((
                            Err(io::Error::other(format!(
                                "{:?}: {}",
                                detail.code, detail.message
                            ))),
                            (conn, 0, delivered, true),
                        ));
                    }
                };
                match item {
                    StreamItem::Data(data) => {
                        if data.sequence != expected_sequence
                            || checksum_block(&data.bytes) != data.checksum
                        {
                            let error = io::Error::other(format!(
                                "body chunk {} out of order or corrupt in transit",
                                data.sequence
                            ));
                            return Some((Err(error), (conn, 0, delivered, true)));
                        }
                        let delivered = delivered + data.bytes.len() as u64;
                        Some((
                            Ok(Bytes::from(data.bytes)),
                            (conn, expected_sequence + 1, delivered, false),
                        ))
                    }
                    // A clean end means the coordinator has already checked
                    // the delivered length and the whole-object checksum
                    // against the record (SPEC 11.7) and would have ended
                    // with ObjectChecksumMismatch otherwise; each chunk was
                    // checked above. This is what `djbod get` relies on too.
                    // It also proves every shard read intact, so any note
                    // of an earlier failure on this key is dropped.
                    StreamItem::End(end) => match end.error {
                        None if end.reconstructed.is_empty() && end.missing_records.is_empty() => {
                            app.clear_failure(&key);
                            None
                        }
                        // Correct bytes, a cluster not whole (SPEC 11.4,
                        // 9.4.4): noted so the page shows it until a repair.
                        None => {
                            app.record_failure(
                                &key,
                                "download",
                                delivered,
                                App::degraded_detail(&end.reconstructed, &end.missing_records),
                            );
                            None
                        }
                        Some(detail) => {
                            app.record_failure(&key, "download", delivered, detail.clone());
                            Some((
                                Err(io::Error::other(format!(
                                    "{:?}: {}",
                                    detail.code, detail.message
                                ))),
                                (conn, 0, delivered, true),
                            ))
                        }
                    },
                }
            }
        },
    );
    let content_type = record
        .content_type
        .as_deref()
        .and_then(|ct| HeaderValue::from_str(ct).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    let file_name: String = key
        .rsplit('/')
        .next()
        .unwrap_or("object")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut response = Body::from_stream(body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(record.size));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        "x-djbod-version",
        HeaderValue::from_str(&record.version.to_text())
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    Ok(response)
}

/// Read the object through, as a download would, without keeping it: the
/// node checks every block against its checksum and the whole object at
/// the end (SPEC 11.7), so this answers whether a download would succeed
/// and, when it would not, exactly what is damaged. The answer streams as
/// JSON lines so the page can show progress: `start` with the size, a
/// `progress` line every few megabytes, then `done` with `verified` and,
/// when false, the node's error detail. A damaged object is a result, not
/// a failure of the request; only not finding the object or the node is
/// an HTTP error.
async fn verify_object(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    const REPORT_EVERY: u64 = 8 * 1024 * 1024;
    let mut conn = connect(&app).await?;
    let id = conn
        .send_request(Request::GetObject { key: key.clone() })
        .await?;
    let record = match conn.read_response(id).await? {
        Reply::GetObject { record } => record,
        other => return Err(ApiError::unexpected(other)),
    };
    let size = record.size;
    let start = json!({
        "event": "start",
        "key": key,
        "version": record.version.to_text(),
        "size": size,
    });
    struct Progress {
        conn: Connection,
        expected_sequence: u64,
        bytes: u64,
        unreported: u64,
        finished: bool,
        app: Arc<App>,
        key: String,
    }
    let state = Progress {
        conn,
        expected_sequence: 0,
        bytes: 0,
        unreported: 0,
        finished: false,
        app: app.clone(),
        key: key.clone(),
    };
    /// The last line: the verdict, remembered or cleared for the key. A
    /// verified body that needed reconstruction (SPEC 11.4) or went
    /// without a record copy (9.4.4) is a cluster not whole, remembered
    /// as such.
    fn done(
        app: &App,
        key: &str,
        verified: bool,
        error: Option<ErrorDetail>,
        reconstructed: Vec<Reconstruction>,
        missing_records: Vec<MissingRecordCopy>,
        bytes: u64,
    ) -> Value {
        match &error {
            None if verified && reconstructed.is_empty() && missing_records.is_empty() => {
                app.clear_failure(key)
            }
            None if verified => app.record_failure(
                key,
                "verify",
                bytes,
                App::degraded_detail(&reconstructed, &missing_records),
            ),
            Some(detail) => app.record_failure(key, "verify", bytes, detail.clone()),
            None => {}
        }
        json!({ "event": "done", "verified": verified, "error": error, "reconstructed": reconstructed, "missing_records": missing_records, "bytes": bytes })
    }
    let lines = futures_util::stream::unfold(state, move |mut st| async move {
        if st.finished {
            return None;
        }
        let line = loop {
            match st.conn.read_stream_item(id).await {
                Ok(StreamItem::Data(data)) => {
                    if data.sequence != st.expected_sequence
                        || checksum_block(&data.bytes) != data.checksum
                    {
                        st.finished = true;
                        let detail = ErrorDetail::new(
                            ErrorCode::ProtocolViolation,
                            format!(
                                "body chunk {} out of order or corrupt in transit",
                                data.sequence
                            ),
                        );
                        break done(
                            &st.app,
                            &st.key,
                            false,
                            Some(detail),
                            Vec::new(),
                            Vec::new(),
                            st.bytes,
                        );
                    }
                    st.expected_sequence += 1;
                    st.bytes += data.bytes.len() as u64;
                    st.unreported += data.bytes.len() as u64;
                    if st.unreported < REPORT_EVERY && st.bytes < size {
                        continue;
                    }
                    st.unreported = 0;
                    break json!({ "event": "progress", "bytes": st.bytes });
                }
                Ok(StreamItem::End(end)) => {
                    st.finished = true;
                    break done(
                        &st.app,
                        &st.key,
                        end.error.is_none(),
                        end.error,
                        end.reconstructed,
                        end.missing_records,
                        st.bytes,
                    );
                }
                Err(e) => {
                    st.finished = true;
                    let detail = match e {
                        ConnectionError::Remote(d) | ConnectionError::StreamFailed(d) => d,
                        other => ErrorDetail::new(ErrorCode::Internal, other.to_string()),
                    };
                    break done(
                        &st.app,
                        &st.key,
                        false,
                        Some(detail),
                        Vec::new(),
                        Vec::new(),
                        st.bytes,
                    );
                }
            }
        };
        let mut text = line.to_string();
        text.push('\n');
        Some((Ok::<Bytes, io::Error>(Bytes::from(text)), st))
    });
    let first = futures_util::stream::once(async move {
        let mut text = start.to_string();
        text.push('\n');
        Ok::<Bytes, io::Error>(Bytes::from(text))
    });
    let mut response = Body::from_stream(first.chain(lines)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn repair_object(State(app): State<Arc<App>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&app).await?;
    match conn
        .request(Request::RepairObject { key: key.clone() })
        .await?
    {
        Reply::RepairObject(report) => {
            app.clear_failure(&key);
            Ok(Json(json!(report)))
        }
        other => Err(ApiError::unexpected(other)),
    }
}

#[derive(Deserialize)]
struct MoveShardBody {
    shard_index: u8,
    to: Option<Uuid>,
}

async fn move_shard(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Json(body): Json<MoveShardBody>,
) -> ApiResult {
    let mut conn = connect(&app).await?;
    let request = Request::MoveShard {
        key,
        shard_index: body.shard_index,
        target: body.to.map(DeviceId),
    };
    match conn.request(request).await? {
        Reply::MoveShard {
            record,
            source,
            source_cleaned,
            rebuilt,
        } => Ok(Json(json!({
            "record": record,
            "source": source,
            "source_cleaned": source_cleaned,
            "rebuilt": rebuilt,
        }))),
        other => Err(ApiError::unexpected(other)),
    }
}

// --------------------------------------------------- streamed operations

/// One line of JSON per event, then one `{"event":"end", ...}` line
/// carrying the stream's end, so the page can show progress as it
/// happens and knows how the operation finished.
fn ndjson_stream<E, F, Fut>(conn: Connection, id: u32, next: F) -> Response
where
    E: Serialize + Send + 'static,
    F: Fn(Connection, u32) -> Fut + Send + 'static,
    Fut: std::future::Future<
            Output = (
                Connection,
                Result<Result<E, djbod_proto::message::StreamEnd>, ConnectionError>,
            ),
        > + Send
        + 'static,
{
    let lines = futures_util::stream::unfold((conn, false), move |(conn, finished)| {
        let next = next(conn, id);
        async move {
            if finished {
                return None;
            }
            let (conn, item) = next.await;
            let (line, finished) = match item {
                Ok(Ok(event)) => (json!(event), false),
                Ok(Err(end)) => (json!({ "event": "end", "error": end.error }), true),
                Err(e) => {
                    let detail = match e {
                        ConnectionError::Remote(d) | ConnectionError::StreamFailed(d) => d,
                        other => ErrorDetail::new(ErrorCode::Internal, other.to_string()),
                    };
                    (json!({ "event": "end", "error": detail }), true)
                }
            };
            let mut text = line.to_string();
            text.push('\n');
            Some((Ok::<Bytes, io::Error>(Bytes::from(text)), (conn, finished)))
        }
    });
    let mut response = Body::from_stream(lines).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
struct ScrubBody {
    rate_mib: Option<u64>,
    #[serde(default)]
    repair: bool,
}

async fn scrub(State(app): State<Arc<App>>, Json(body): Json<ScrubBody>) -> ApiResult<Response> {
    let mut conn = connect(&app).await?;
    let id = conn
        .start_scrub(body.rate_mib.map(|m| m * 1024 * 1024), body.repair)
        .await?;
    Ok(ndjson_stream(conn, id, |mut conn, id| async move {
        let item = conn.next_scrub_event(id).await;
        (conn, item)
    }))
}

#[derive(Deserialize)]
struct DrainBody {
    #[serde(default)]
    partial: bool,
}

async fn drain(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<DrainBody>,
) -> ApiResult<Response> {
    let device = device_param(&app, &id).await?;
    let mut conn = connect(&app).await?;
    let id = conn.start_drain(device, body.partial).await?;
    Ok(ndjson_stream(conn, id, |mut conn, id| async move {
        let item = conn.next_drain_event(id).await;
        (conn, item)
    }))
}

// ------------------------------------------------------------ membership

#[derive(Deserialize)]
struct StateBody {
    state: DeviceState,
}

async fn set_device_state(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> ApiResult {
    let target = &app.target;
    let device = device_param(&app, &id).await?;
    if body.state == DeviceState::Removed {
        return Err(ApiError::BadRequest(
            "a device is removed with the remove action, after draining".to_string(),
        ));
    }
    let (document, changed) = admin::set_device_state(
        &target.connector,
        app.peer().await?,
        target.cluster,
        device,
        body.state,
    )
    .await?;
    Ok(Json(json!({
        "device": device,
        "state": body.state,
        "document_version": document.version,
        "changed": changed,
    })))
}

#[derive(Deserialize)]
struct LabelBody {
    /// The new label, or null to clear it.
    label: Option<String>,
}

/// Give a device a name shown beside its UUID, or clear it (SPEC
/// 6.2.5.1). The document validates the label: 1 to 128 bytes, no
/// whitespace, unique, not shaped like a UUID.
async fn set_device_label(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<LabelBody>,
) -> ApiResult {
    let target = &app.target;
    let device = device_param(&app, &id).await?;
    let label = body
        .label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let (document, changed) = admin::set_device_label(
        &target.connector,
        app.peer().await?,
        target.cluster,
        device,
        label.clone(),
    )
    .await?;
    Ok(Json(json!({
        "device": device,
        "label": label,
        "document_version": document.version,
        "changed": changed,
    })))
}

async fn remove_device(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let target = &app.target;
    let device = device_param(&app, &id).await?;
    let (document, changed) =
        admin::remove_device(&target.connector, app.peer().await?, target.cluster, device).await?;
    Ok(Json(json!({
        "device": device,
        "document_version": document.version,
        "changed": changed,
    })))
}

/// Give a node a name shown beside its UUID, or clear it (SPEC 6.2.5.1);
/// the document validates it as it does a device label.
async fn set_node_label(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<LabelBody>,
) -> ApiResult {
    let target = &app.target;
    let node = NodeId(parse_id(&id, "node")?);
    let label = body
        .label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let (document, changed) = admin::set_node_label(
        &target.connector,
        app.peer().await?,
        target.cluster,
        node,
        label.clone(),
    )
    .await?;
    Ok(Json(json!({
        "node": node,
        "label": label,
        "document_version": document.version,
        "changed": changed,
    })))
}

async fn remove_node(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let target = &app.target;
    let node = NodeId(parse_id(&id, "node")?);
    let document =
        admin::remove_node(&target.connector, app.peer().await?, target.cluster, node).await?;
    Ok(Json(json!({
        "node": node,
        "document_version": document.version,
    })))
}

#[cfg(test)]
mod peers {
    use super::candidates;
    use std::net::SocketAddr;

    fn a(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn preferred_then_configured_then_learned_each_once() {
        let configured = [a("10.0.0.1:5263"), a("10.0.0.2:5263")];
        let learned = [a("10.0.0.2:5263"), a("10.0.0.3:5263"), a("10.0.0.1:5263")];
        assert_eq!(
            candidates(Some(a("10.0.0.2:5263")), &configured, &learned),
            vec![a("10.0.0.2:5263"), a("10.0.0.1:5263"), a("10.0.0.3:5263")]
        );
        assert_eq!(
            candidates(None, &configured, &[]),
            vec![a("10.0.0.1:5263"), a("10.0.0.2:5263")]
        );
    }
}
