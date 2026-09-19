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
//! | `GET  /cluster`                      | document plus every node's version  |
//! | `POST /cluster/sync`                 | `djbod cluster sync`                |
//! | `POST /cluster/scheme`               | `djbod cluster set-scheme`          |
//! | `POST /cluster/limits`               | `djbod cluster set-limits`          |
//! | `GET  /objects?prefix&start_after&limit` | `ListKeys`                      |
//! | `GET  /objects/{key}`                | `HeadObject`                        |
//! | `PUT  /objects/{key}`                | `PutObject`, body streamed through  |
//! | `DELETE /objects/{key}`              | `DeleteObject`                      |
//! | `GET  /download/{key}`               | `GetObject`, body streamed through  |
//! | `POST /repair/{key}`                 | `RepairObject`                      |
//! | `POST /move-shard/{key}`             | `MoveShard`                         |
//! | `POST /scrub`                        | `Scrub`, events streamed as NDJSON  |
//! | `POST /devices/{id}/state`           | `djbod cluster set-state`           |
//! | `POST /devices/{id}/drain`           | `Drain`, events streamed as NDJSON  |
//! | `POST /devices/{id}/remove`          | `djbod cluster remove-device`       |
//! | `POST /nodes/{id}/remove`            | `djbod cluster remove-node`         |
//!
//! Errors are JSON: `{"error": {"code", "message", ...}}`, where the
//! fields are those of the node's `ErrorDetail` (SPEC 16.2) when the
//! node refused, so the administrator sees the same identifying detail
//! the command line prints.
//!
//! There is no authentication, as there is none on the native protocol
//! yet (19.1.6); the server binds to localhost by default.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::io::StreamReader;
use uuid::Uuid;

use djbod_core::checksum::checksum_block;
use djbod_core::cluster::{DeviceState, NodeId};
use djbod_core::record::DeviceId;
use djbod_node::client::{ClientError, Connection, StreamItem, DEFAULT_BODY_CHUNK};
use djbod_node::membership::{self, MembershipError};
use djbod_proto::message::{ErrorCode, ErrorDetail, ListQuery, Request, Response as Reply};

/// The page, embedded so the binary is self-contained.
pub const PAGE: &str = include_str!("../ui.html");

/// Which cluster the UI administers: any node's address and the cluster
/// id, the same two values the command-line client needs.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub node: SocketAddr,
    pub cluster: Uuid,
}

/// The whole application: the page at `/` and the API under `/api`.
pub fn router(target: Target) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/cluster", get(cluster))
        .route("/cluster/sync", post(cluster_sync))
        .route("/cluster/scheme", post(cluster_scheme))
        .route("/cluster/limits", post(cluster_limits))
        .route("/objects", get(list_objects))
        .route(
            "/objects/{*key}",
            get(head_object).put(put_object).delete(delete_object),
        )
        .route("/download/{*key}", get(download_object))
        .route("/repair/{*key}", post(repair_object))
        .route("/move-shard/{*key}", post(move_shard))
        .route("/scrub", post(scrub))
        .route("/devices/{id}/state", post(set_device_state))
        .route("/devices/{id}/drain", post(drain))
        .route("/devices/{id}/remove", post(remove_device))
        .route("/nodes/{id}/remove", post(remove_node))
        // Object bodies are as large as the cluster allows, not as large
        // as axum's default two megabytes.
        .layer(DefaultBodyLimit::disable());
    Router::new()
        .route("/", get(page))
        .nest("/api", api)
        .with_state(Arc::new(target))
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

// ---------------------------------------------------------------- errors

/// What a handler can fail with, and how each is reported.
#[derive(Debug)]
pub enum ApiError {
    /// The node answered with an error; reported with every field.
    Remote(ErrorDetail),
    /// The node could not be reached or talked to.
    Client(Box<ClientError>),
    /// A document change failed.
    Membership(Box<MembershipError>),
    /// The request itself was wrong.
    BadRequest(String),
}

impl From<ClientError> for ApiError {
    fn from(e: ClientError) -> ApiError {
        match e {
            ClientError::Remote(detail) | ClientError::StreamFailed(detail) => {
                ApiError::Remote(detail)
            }
            other => ApiError::Client(Box::new(other)),
        }
    }
}

impl From<MembershipError> for ApiError {
    fn from(e: MembershipError) -> ApiError {
        ApiError::Membership(Box::new(e))
    }
}

impl ApiError {
    fn unexpected(reply: Reply) -> ApiError {
        ApiError::Client(Box::new(ClientError::UnexpectedMessage {
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
                    _ => StatusCode::BAD_GATEWAY,
                };
                (status, json!({ "error": detail }))
            }
            ApiError::Client(e) => (
                StatusCode::BAD_GATEWAY,
                json!({ "error": { "code": "node_unreachable", "message": e.to_string() } }),
            ),
            ApiError::Membership(e) => (
                StatusCode::BAD_GATEWAY,
                json!({ "error": { "code": "membership", "message": e.to_string() } }),
            ),
            ApiError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": { "code": "bad_request", "message": message } }),
            ),
        };
        (status, Json(body)).into_response()
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

async fn connect(target: &Target) -> ApiResult<Connection> {
    Ok(Connection::connect(target.node, Connection::client_hello(target.cluster)).await?)
}

fn parse_id(id: &str, what: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(id).map_err(|_| ApiError::BadRequest(format!("{what} id {id:?} is not a UUID")))
}

// ---------------------------------------------------------------- status

async fn status(State(target): State<Arc<Target>>) -> ApiResult {
    let mut conn = connect(&target).await?;
    match conn.request(Request::Status).await? {
        // `..`: the status gains fields as the protocol grows (the TLS
        // work adds `transport`); the page needs only these.
        Reply::Status {
            cluster_id,
            document_version,
            coordinator,
            devices,
            ..
        } => Ok(Json(json!({
            "cluster_id": cluster_id,
            "document_version": document_version,
            "coordinator": coordinator,
            "devices": devices,
        }))),
        other => Err(ApiError::unexpected(other)),
    }
}

/// The document as the target node holds it, and what every node listed
/// in it answers when asked for its own version (`djbod cluster show`).
async fn cluster(State(target): State<Arc<Target>>) -> ApiResult {
    let document = membership::fetch_document(target.node, target.cluster).await?;
    let reports = membership::fetch_all(&document).await;
    let nodes: Vec<Value> = reports
        .iter()
        .map(|r| {
            json!({
                "node": r.node,
                "address": r.address,
                "version": r.result.as_ref().ok().map(|d| d.version),
                "error": r.result.as_ref().err(),
            })
        })
        .collect();
    Ok(Json(json!({ "document": document, "nodes": nodes })))
}

async fn cluster_sync(State(target): State<Arc<Target>>) -> ApiResult {
    let report = membership::sync(target.node, target.cluster).await?;
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

async fn cluster_scheme(
    State(target): State<Arc<Target>>,
    Json(body): Json<SchemeBody>,
) -> ApiResult {
    let (document, changed) =
        membership::set_scheme(target.node, target.cluster, body.k, body.m, body.block_size)
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

async fn cluster_limits(
    State(target): State<Arc<Target>>,
    Json(body): Json<LimitsBody>,
) -> ApiResult {
    if body.max_key_bytes.is_none()
        && body.max_object_bytes.is_none()
        && body.max_user_metadata_bytes.is_none()
    {
        return Err(ApiError::BadRequest("no limit given".to_string()));
    }
    let (document, changed) = membership::set_limits(
        target.node,
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

async fn list_objects(
    State(target): State<Arc<Target>>,
    Query(params): Query<ListParams>,
) -> ApiResult {
    let mut conn = connect(&target).await?;
    let query = ListQuery {
        prefix: params.prefix.filter(|p| !p.is_empty()),
        start_after: params.start_after.filter(|s| !s.is_empty()),
        limit: params.limit,
    };
    match conn.request(Request::ListKeys(query)).await? {
        Reply::ListKeys { keys, truncated } => {
            Ok(Json(json!({ "keys": keys, "truncated": truncated })))
        }
        other => Err(ApiError::unexpected(other)),
    }
}

async fn head_object(State(target): State<Arc<Target>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&target).await?;
    match conn.request(Request::HeadObject { key }).await? {
        Reply::HeadObject { record } => Ok(Json(json!(record))),
        other => Err(ApiError::unexpected(other)),
    }
}

/// Store the request body under the key. The size must be known up front
/// (SPEC 10.1), so `Content-Length` is required; browsers send it for a
/// `File` body. The body is streamed to the node a chunk at a time.
async fn put_object(
    State(target): State<Arc<Target>>,
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
    let mut conn = connect(&target).await?;
    let mut source = StreamReader::new(body.into_data_stream().map_err(io::Error::other));
    let version = conn
        .put_object_from_reader(&key, size, &mut source, DEFAULT_BODY_CHUNK, content_type)
        .await?;
    Ok(Json(json!({ "key": key, "version": version.to_text() })))
}

async fn delete_object(State(target): State<Arc<Target>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&target).await?;
    match conn
        .request(Request::DeleteObject { key: key.clone() })
        .await?
    {
        Reply::DeleteObject => Ok(Json(json!({ "key": key, "deleted": true }))),
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
    State(target): State<Arc<Target>>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    let mut conn = connect(&target).await?;
    let id = conn
        .send_request(Request::GetObject { key: key.clone() })
        .await?;
    let record = match conn.read_response(id).await? {
        Reply::GetObject { record } => record,
        other => return Err(ApiError::unexpected(other)),
    };
    let body = futures_util::stream::unfold(
        (conn, 0u64, false),
        move |(mut conn, expected_sequence, finished)| async move {
            if finished {
                return None;
            }
            let item = match conn.read_stream_item(id).await {
                Ok(item) => item,
                Err(e) => return Some((Err(io::Error::other(e.to_string())), (conn, 0, true))),
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
                        return Some((Err(error), (conn, 0, true)));
                    }
                    Some((
                        Ok(Bytes::from(data.bytes)),
                        (conn, expected_sequence + 1, false),
                    ))
                }
                StreamItem::End(end) => match end.error {
                    None => None,
                    Some(detail) => Some((
                        Err(io::Error::other(format!(
                            "{:?}: {}",
                            detail.code, detail.message
                        ))),
                        (conn, 0, true),
                    )),
                },
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

async fn repair_object(State(target): State<Arc<Target>>, Path(key): Path<String>) -> ApiResult {
    let mut conn = connect(&target).await?;
    match conn.request(Request::RepairObject { key }).await? {
        Reply::RepairObject(report) => Ok(Json(json!(report))),
        other => Err(ApiError::unexpected(other)),
    }
}

#[derive(Deserialize)]
struct MoveShardBody {
    shard_index: u8,
    to: Option<Uuid>,
}

async fn move_shard(
    State(target): State<Arc<Target>>,
    Path(key): Path<String>,
    Json(body): Json<MoveShardBody>,
) -> ApiResult {
    let mut conn = connect(&target).await?;
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
                Result<Result<E, djbod_proto::message::StreamEnd>, ClientError>,
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
                        ClientError::Remote(d) | ClientError::StreamFailed(d) => d,
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

async fn scrub(
    State(target): State<Arc<Target>>,
    Json(body): Json<ScrubBody>,
) -> ApiResult<Response> {
    let mut conn = connect(&target).await?;
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
    State(target): State<Arc<Target>>,
    Path(id): Path<String>,
    Json(body): Json<DrainBody>,
) -> ApiResult<Response> {
    let device = DeviceId(parse_id(&id, "device")?);
    let mut conn = connect(&target).await?;
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
    State(target): State<Arc<Target>>,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> ApiResult {
    let device = DeviceId(parse_id(&id, "device")?);
    if body.state == DeviceState::Removed {
        return Err(ApiError::BadRequest(
            "a device is removed with the remove action, after draining".to_string(),
        ));
    }
    let (document, changed) =
        membership::set_device_state(target.node, target.cluster, device, body.state).await?;
    Ok(Json(json!({
        "device": device,
        "state": body.state,
        "document_version": document.version,
        "changed": changed,
    })))
}

async fn remove_device(State(target): State<Arc<Target>>, Path(id): Path<String>) -> ApiResult {
    let device = DeviceId(parse_id(&id, "device")?);
    let (document, changed) =
        membership::remove_device(target.node, target.cluster, device).await?;
    Ok(Json(json!({
        "device": device,
        "document_version": document.version,
        "changed": changed,
    })))
}

async fn remove_node(State(target): State<Arc<Target>>, Path(id): Path<String>) -> ApiResult {
    let node = NodeId(parse_id(&id, "node")?);
    let document = membership::remove_node(target.node, target.cluster, node).await?;
    Ok(Json(json!({
        "node": node,
        "document_version": document.version,
    })))
}
