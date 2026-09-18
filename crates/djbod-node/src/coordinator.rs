//! The client-facing operations (SPEC 19.1.3), served by the node a
//! client connects to. The coordinator does everything by sending
//! node-to-node operations to the nodes in the cluster document, including
//! itself over the loopback interface, so there is one code path whether
//! the cluster has one node or twenty (4.1).
//!
//! Fail-stop (16): any node that does not answer, any device that cannot
//! be reached, any checksum that does not match, fails the request with
//! an error carrying the fields of 16.2.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use time::OffsetDateTime;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::Xxh3;

use djbod_core::checksum::{checksum_block, BlockChecksum};
use djbod_core::cluster::{DeviceState, NodeId};
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::{hash_key, KeyHash};
use djbod_core::record::{
    DeviceId, MetadataRecord, ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME,
};
use djbod_core::shardfile::{shard_file_length, shard_geometry};
use djbod_core::stripe::{decode_stripe, encode_stripe, DecodedStripe, ShardBlock};
use djbod_core::version::VersionId;
use djbod_proto::message::{
    DataFrame, DeviceStatus, ErrorCode, ErrorDetail, KeyEntry, ListQuery, LocatedRecord, Message,
    RepairReport, Request, Response, ShardCondition, ShardRepair, StreamEnd,
};

use crate::client::{ClientError, Connection, StreamItem};
use crate::local_ops::{respond, Failure};
use crate::node::Node;
use crate::server::{our_hello, ConnectionEnd, Reader, Writer};
use crate::ulid::VersionGenerator;
use crate::wire::{read_message, write_message};

/// Sanity limit on key length (SPEC 9.1.5).
pub const MAX_KEY_BYTES: usize = 16 * 1024;
/// Maximum object size (SPEC 9.3.1).
pub const MAX_OBJECT_BYTES: u64 = 1 << 40;

pub fn is_client_operation(request: &Request) -> bool {
    matches!(
        request,
        Request::Status
            | Request::PutObject { .. }
            | Request::GetObject { .. }
            | Request::HeadObject { .. }
            | Request::DeleteObject { .. }
            | Request::ListKeys(_)
            | Request::RepairObject { .. }
    )
}

/// Serve one client operation.
pub async fn handle(
    node: &Arc<Node>,
    versions: &VersionGenerator,
    id: u32,
    request: Request,
    reader: &mut Reader,
    writer: &mut Writer,
) -> Result<(), ConnectionEnd> {
    let outcome: Result<(), Failure> = match request {
        Request::Status => respond(writer, id, status(node).await).await,
        Request::HeadObject { key } => respond(writer, id, head_object(node, &key).await).await,
        Request::DeleteObject { key } => respond(writer, id, delete_object(node, &key).await).await,
        Request::ListKeys(query) => respond(writer, id, list_keys(node, query).await).await,
        Request::RepairObject { key } => respond(writer, id, repair_object(node, &key).await).await,
        Request::GetObject { key } => get_object(node, id, writer, &key).await,
        Request::PutObject {
            key,
            size,
            content_type,
            user_metadata,
        } => {
            put_object(
                node,
                versions,
                id,
                reader,
                writer,
                PutParams {
                    key,
                    size,
                    content_type,
                    user_metadata,
                },
            )
            .await
        }
        other => {
            respond(
                writer,
                id,
                Err(Failure::Error(ErrorDetail::new(
                    ErrorCode::ProtocolViolation,
                    format!("{other:?} is not a client operation"),
                ))),
            )
            .await
        }
    };
    match outcome {
        Ok(()) | Err(Failure::Error(_)) => Ok(()),
        Err(Failure::Close(end)) => Err(end),
    }
}

// --------------------------------------------------------- connections

fn error(code: ErrorCode, message: impl Into<String>) -> Failure {
    Failure::Error(ErrorDetail::new(code, message))
}

fn node_address(node: &Node, target: NodeId) -> Result<SocketAddr, Failure> {
    let document = node.document();
    let entry = document.node(target).ok_or_else(|| {
        error(
            ErrorCode::NodeUnreachable,
            format!(
                "{target} is not in cluster document version {}",
                document.version
            ),
        )
    })?;
    let first = entry.addresses.first().ok_or_else(|| {
        error(
            ErrorCode::NodeUnreachable,
            format!("{target} has no address"),
        )
    })?;
    first.parse().map_err(|e| {
        error(
            ErrorCode::NodeUnreachable,
            format!("{target} address {first:?} is not valid: {e}"),
        )
    })
}

/// Open a node-to-node connection to `target`, which may be this node.
async fn connect_to(node: &Node, target: NodeId) -> Result<Connection, Failure> {
    let address = node_address(node, target)?;
    Connection::connect(address, our_hello(node))
        .await
        .map_err(|e| {
            Failure::Error(ErrorDetail {
                node: Some(target),
                ..ErrorDetail::new(
                    ErrorCode::NodeUnreachable,
                    format!("cannot reach {target} at {address}: {e}"),
                )
            })
        })
}

fn remote_failure(target: NodeId, e: ClientError) -> Failure {
    match e {
        ClientError::Remote(detail) => Failure::Error(ErrorDetail {
            node: detail.node.or(Some(target)),
            ..detail
        }),
        ClientError::StreamFailed(detail) => Failure::Error(ErrorDetail {
            node: detail.node.or(Some(target)),
            ..detail
        }),
        other => Failure::Error(ErrorDetail {
            node: Some(target),
            ..ErrorDetail::new(
                ErrorCode::NodeUnreachable,
                format!("{target} failed mid-request: {other}"),
            )
        }),
    }
}

/// Send one request to every node in the document and collect the
/// responses. Any node failing fails the whole call (13.3, 16.1).
async fn broadcast(node: &Arc<Node>, request: Request) -> Result<Vec<(NodeId, Response)>, Failure> {
    let document = node.document();
    let mut tasks = JoinSet::new();
    for entry in &document.nodes {
        let target = entry.id;
        let node = node.clone();
        let request = request.clone();
        tasks.spawn(async move {
            let mut connection = connect_to(&node, target).await?;
            let response = connection
                .request(request)
                .await
                .map_err(|e| remote_failure(target, e))?;
            Ok::<(NodeId, Response), Failure>((target, response))
        });
    }
    let mut responses = Vec::with_capacity(document.nodes.len());
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(pair)) => responses.push(pair),
            Ok(Err(f)) => return Err(f),
            Err(e) => {
                return Err(error(
                    ErrorCode::Internal,
                    format!("broadcast task failed: {e}"),
                ))
            }
        }
    }
    responses.sort_by_key(|(id, _)| *id);
    Ok(responses)
}

// ------------------------------------------------------------- lookups

/// The broadcast lookup of section 13: every record copy for a key hash,
/// from every node.
async fn lookup(node: &Arc<Node>, key_hash: KeyHash) -> Result<Vec<LocatedRecord>, Failure> {
    let mut located = Vec::new();
    for (target, response) in broadcast(node, Request::LocalLookup { key_hash }).await? {
        match response {
            Response::LocalLookup { records } => located.extend(records),
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalLookup with {other:?}"),
                ))
            }
        }
    }
    Ok(located)
}

/// Group record copies by version, newest first, checking each version's
/// copies agree, are complete, come from their listed holders, and name
/// the requested key (9.1.6, 9.4.4).
fn versions_of(key: &str, located: Vec<LocatedRecord>) -> Result<Vec<MetadataRecord>, Failure> {
    let mut by_version: BTreeMap<VersionId, Vec<LocatedRecord>> = BTreeMap::new();
    for item in located {
        by_version
            .entry(item.record.version)
            .or_default()
            .push(item);
    }
    let mut versions = Vec::with_capacity(by_version.len());
    for (version, copies) in by_version.into_iter().rev() {
        let first = &copies[0].record;
        if first.key != key {
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(version),
                ..ErrorDetail::new(
                    ErrorCode::KeyMismatch,
                    format!(
                        "record under this key hash names key {:?}, not {key:?}: hash collision or corruption",
                        first.key
                    ),
                )
            }));
        }
        let expected = first.k as usize + first.m as usize;
        if copies.len() != expected {
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(version),
                ..ErrorDetail::new(
                    ErrorCode::RecordsInconsistent,
                    format!("{} record copies found, {expected} expected", copies.len()),
                )
            }));
        }
        for copy in &copies {
            if copy.record != *first {
                return Err(Failure::Error(ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(version),
                    device: Some(copy.device),
                    ..ErrorDetail::new(
                        ErrorCode::RecordsInconsistent,
                        "record copies disagree".to_string(),
                    )
                }));
            }
            if first.shard_on(copy.device).is_none() {
                return Err(Failure::Error(ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(version),
                    device: Some(copy.device),
                    ..ErrorDetail::new(
                        ErrorCode::RecordsInconsistent,
                        "a record copy was found on a device the record does not list".to_string(),
                    )
                }));
            }
        }
        versions.push(first.clone());
    }
    Ok(versions)
}

fn check_key(key: &str) -> Result<(), Failure> {
    if key.is_empty() {
        return Err(error(ErrorCode::ProtocolViolation, "key is empty"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(Failure::Error(ErrorDetail {
            key: Some(key[..64].to_string() + "..."),
            ..ErrorDetail::new(
                ErrorCode::KeyTooLong,
                format!("key is {} bytes; the limit is {MAX_KEY_BYTES}", key.len()),
            )
        }));
    }
    Ok(())
}

/// The newest version of `key`, or NotFound.
async fn newest_version(node: &Arc<Node>, key: &str) -> Result<MetadataRecord, Failure> {
    check_key(key)?;
    let located = lookup(node, hash_key(key.as_bytes())).await?;
    let versions = versions_of(key, located)?;
    versions.into_iter().next().ok_or_else(|| {
        Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            ..ErrorDetail::new(ErrorCode::NotFound, format!("no object under key {key:?}"))
        })
    })
}

// ------------------------------------------------------------ handlers

async fn status(node: &Arc<Node>) -> Result<Response, Failure> {
    let document = node.document();
    let mut devices: Vec<DeviceStatus> = Vec::new();
    for (target, response) in broadcast(node, Request::LocalStatus).await? {
        match response {
            Response::LocalStatus {
                document_version,
                devices: found,
                ..
            } => {
                if document_version != document.version {
                    return Err(Failure::Error(ErrorDetail {
                        node: Some(target),
                        ..ErrorDetail::new(
                            ErrorCode::DocumentVersionMismatch,
                            format!(
                                "{target} holds document version {document_version}, this node {}",
                                document.version
                            ),
                        )
                    }));
                }
                devices.extend(found);
            }
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalStatus with {other:?}"),
                ))
            }
        }
    }
    // Grouped by node; within a node, the order that node listed them,
    // which is its configuration order.
    devices.sort_by_key(|d| d.node);
    Ok(Response::Status {
        cluster_id: document.cluster_id,
        document_version: document.version,
        coordinator: node.id(),
        devices,
    })
}

async fn head_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    let record = newest_version(node, key).await?;
    Ok(Response::HeadObject { record })
}

async fn delete_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    check_key(key)?;
    let key_hash = hash_key(key.as_bytes());
    let located = lookup(node, key_hash).await?;
    let versions = versions_of(key, located)?;
    if versions.is_empty() {
        return Err(Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            ..ErrorDetail::new(ErrorCode::NotFound, format!("no object under key {key:?}"))
        }));
    }
    for record in &versions {
        delete_version_everywhere(node, record).await?;
    }
    Ok(Response::DeleteObject)
}

/// Remove one version from every holder (14.1). Any unreachable holder
/// fails the delete; a repeat succeeds because deletion is idempotent.
async fn delete_version_everywhere(
    node: &Arc<Node>,
    record: &MetadataRecord,
) -> Result<(), Failure> {
    let document = node.document();
    for shard in &record.shards {
        let owner = document
            .device(shard.device)
            .map(|d| d.node)
            .ok_or_else(|| {
                Failure::Error(ErrorDetail {
                    device: Some(shard.device),
                    key: Some(record.key.clone()),
                    version: Some(record.version),
                    ..ErrorDetail::new(
                        ErrorCode::DeviceUnavailable,
                        format!("{} is not in the cluster document", shard.device),
                    )
                })
            })?;
        let mut connection = connect_to(node, owner).await?;
        connection
            .request(Request::DeleteVersion {
                device: shard.device,
                key_hash: record.key_hash,
                version: record.version,
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
    }
    Ok(())
}

async fn list_keys(node: &Arc<Node>, query: ListQuery) -> Result<Response, Failure> {
    // v1: collect everything matching the prefix from every node, keep
    // the newest version per key, sort, then page (15.1, 15.2.1).
    let mut newest: BTreeMap<String, KeyEntry> = BTreeMap::new();
    let local = ListQuery {
        prefix: query.prefix.clone(),
        start_after: None,
        limit: None,
    };
    for (target, response) in broadcast(node, Request::LocalList(local)).await? {
        match response {
            Response::LocalList { entries } => {
                for entry in entries {
                    match newest.get(&entry.key) {
                        Some(existing) if existing.version >= entry.version => {}
                        _ => {
                            newest.insert(entry.key.clone(), entry);
                        }
                    }
                }
            }
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalList with {other:?}"),
                ))
            }
        }
    }
    let mut keys: Vec<KeyEntry> = newest.into_values().collect();
    if let Some(after) = &query.start_after {
        keys.retain(|e| e.key.as_str() > after.as_str());
    }
    let mut truncated = false;
    if let Some(limit) = query.limit {
        if keys.len() > limit as usize {
            keys.truncate(limit as usize);
            truncated = true;
        }
    }
    Ok(Response::ListKeys { keys, truncated })
}

// ------------------------------------------------------------------ GET

/// Report a failure to the client and close the connection. Used once a
/// stream is in progress in either direction, when a plain error response
/// would leave the two sides out of step.
async fn fail_and_close(writer: &mut Writer, id: u32, detail: ErrorDetail) -> Failure {
    let _ = write_message(
        writer,
        &Message::EndOfStream {
            id,
            end: StreamEnd::failed(detail.clone()),
        },
    )
    .await;
    Failure::Close(ConnectionEnd::ProtocolViolation(detail.message))
}

struct ShardSource {
    index: ShardIndex,
    device: DeviceId,
    owner: NodeId,
    connection: Connection,
    request_id: u32,
}

async fn get_object(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    key: &str,
) -> Result<(), Failure> {
    let record = match newest_version(node, key).await {
        Ok(record) => record,
        Err(f) => return respond(writer, id, Err(f)).await,
    };
    let scheme = match record.scheme() {
        Ok(scheme) => scheme,
        Err(e) => {
            return respond(
                writer,
                id,
                Err(error(ErrorCode::RecordsInconsistent, e.to_string())),
            )
            .await
        }
    };
    let document = node.document();

    // Open one connection per data shard and start every block streaming
    // before answering the client, so a missing holder fails the request
    // cleanly rather than mid-body (11.4).
    let mut sources: Vec<ShardSource> = Vec::with_capacity(scheme.data_shards());
    if record.size > 0 {
        let geometry = match shard_geometry(scheme, record.block_size, record.size) {
            Some(g) => g,
            None => {
                return respond(
                    writer,
                    id,
                    Err(error(
                        ErrorCode::RecordsInconsistent,
                        "record has an impossible size",
                    )),
                )
                .await
            }
        };
        for index in scheme.data_shard_indices() {
            let device = record
                .device_for(index)
                .expect("validated record lists every index");
            let owner = match document.device(device).map(|d| d.node) {
                Some(owner) => owner,
                None => {
                    return respond(
                        writer,
                        id,
                        Err(Failure::Error(ErrorDetail {
                            device: Some(device),
                            key: Some(key.to_string()),
                            version: Some(record.version),
                            shard_index: Some(index.0),
                            ..ErrorDetail::new(
                                ErrorCode::DeviceUnavailable,
                                format!("{device} is not in the cluster document"),
                            )
                        })),
                    )
                    .await
                }
            };
            let mut connection = match connect_to(node, owner).await {
                Ok(c) => c,
                Err(f) => return respond(writer, id, Err(f)).await,
            };
            let request_id = match connection
                .send_request(Request::GetShard {
                    device,
                    key_hash: record.key_hash,
                    version: record.version,
                    shard_index: index.0,
                    first_block: 0,
                    block_count: geometry.block_count,
                })
                .await
            {
                Ok(id) => id,
                Err(e) => return respond(writer, id, Err(remote_failure(owner, e))).await,
            };
            match connection.read_response(request_id).await {
                Ok(Response::GetShard { .. }) => {}
                Ok(other) => {
                    return respond(
                        writer,
                        id,
                        Err(error(
                            ErrorCode::ProtocolViolation,
                            format!("{owner} answered GetShard with {other:?}"),
                        )),
                    )
                    .await
                }
                Err(e) => return respond(writer, id, Err(remote_failure(owner, e))).await,
            }
            sources.push(ShardSource {
                index,
                device,
                owner,
                connection,
                request_id,
            });
        }
    }

    respond(
        writer,
        id,
        Ok(Response::GetObject {
            record: record.clone(),
        }),
    )
    .await?;

    let code = ReedSolomonCode::new(scheme);
    let data_indices = scheme.data_shard_indices();
    let stripe_size = scheme.data_shards() as u64 * record.block_size;
    let stripe_count = record.size.div_ceil(stripe_size.max(1));
    let mut hasher = Xxh3::new();
    let mut delivered: u64 = 0;
    let mut sequence: u64 = 0;
    for stripe in 0..stripe_count {
        let mut received: Vec<ShardBlock> = Vec::with_capacity(sources.len());
        for source in sources.iter_mut() {
            match source.connection.read_stream_item(source.request_id).await {
                Ok(StreamItem::Data(data)) => {
                    if data.sequence != stripe {
                        let detail = ErrorDetail {
                            node: Some(source.owner),
                            device: Some(source.device),
                            key: Some(key.to_string()),
                            version: Some(record.version),
                            shard_index: Some(source.index.0),
                            stripe: Some(stripe),
                            ..ErrorDetail::new(
                                ErrorCode::ProtocolViolation,
                                format!(
                                    "holder sent block {} when {stripe} was expected",
                                    data.sequence
                                ),
                            )
                        };
                        return Err(fail_and_close(writer, id, detail).await);
                    }
                    received.push(ShardBlock {
                        index: source.index,
                        bytes: data.bytes,
                        checksum: data.checksum,
                    });
                }
                Ok(StreamItem::End(end)) => {
                    let detail = ErrorDetail {
                        node: Some(source.owner),
                        device: Some(source.device),
                        key: Some(key.to_string()),
                        version: Some(record.version),
                        shard_index: Some(source.index.0),
                        stripe: Some(stripe),
                        ..end.error.unwrap_or_else(|| {
                            ErrorDetail::new(
                                ErrorCode::ProtocolViolation,
                                "holder ended its stream early".to_string(),
                            )
                        })
                    };
                    return Err(fail_and_close(writer, id, detail).await);
                }
                Err(e) => {
                    let Failure::Error(detail) = remote_failure(source.owner, e) else {
                        unreachable!("remote_failure always yields an error detail")
                    };
                    return Err(fail_and_close(writer, id, detail).await);
                }
            }
        }
        let stripe_len = (record.size - stripe * stripe_size).min(stripe_size) as usize;
        let decoded = match decode_stripe(&code, &data_indices, &received, stripe_len) {
            Ok(decoded) => decoded,
            Err(e) => {
                let detail = ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(ErrorCode::Internal, e.to_string())
                };
                return Err(fail_and_close(writer, id, detail).await);
            }
        };
        let data = match decoded {
            DecodedStripe::Intact { data } => data,
            DecodedStripe::Repaired { faults, .. }
            | DecodedStripe::Unrecoverable { faults, .. } => {
                // Fail-stop (11.4): no reconstruction is served in v1.
                let first = &faults[0];
                let device = record.device_for(first.index);
                let detail = ErrorDetail {
                    device,
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(first.index.0),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(
                        ErrorCode::BlockChecksumMismatch,
                        format!(
                            "{} damaged block(s) in stripe {stripe}; first: {:?}",
                            faults.len(),
                            first.kind
                        ),
                    )
                };
                return Err(fail_and_close(writer, id, detail).await);
            }
        };
        hasher.update(&data);
        delivered += data.len() as u64;
        // Body frames are at most one block long so no frame exceeds the
        // protocol's payload limit.
        for chunk in data.chunks(record.block_size as usize) {
            write_message(
                writer,
                &Message::Data {
                    id,
                    data: DataFrame {
                        sequence,
                        checksum: checksum_block(chunk),
                        bytes: chunk.to_vec(),
                    },
                },
            )
            .await?;
            sequence += 1;
        }
    }

    // Holders end their streams; a holder reporting an error here means
    // the data above was served from a stream that then failed, which
    // cannot happen after all blocks arrived, but check anyway.
    for source in sources.iter_mut() {
        match source.connection.read_stream_item(source.request_id).await {
            Ok(StreamItem::End(end)) if end.error.is_none() => {}
            other => {
                let detail = ErrorDetail {
                    node: Some(source.owner),
                    device: Some(source.device),
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(source.index.0),
                    ..ErrorDetail::new(
                        ErrorCode::ProtocolViolation,
                        format!("holder did not end its stream cleanly: {other:?}"),
                    )
                };
                return Err(fail_and_close(writer, id, detail).await);
            }
        }
    }

    // The whole-object check (11.7), delivered as the stream's status.
    let computed = BlockChecksum(hasher.digest());
    if delivered != record.size || computed != record.object_checksum {
        let detail = ErrorDetail {
            key: Some(key.to_string()),
            version: Some(record.version),
            ..ErrorDetail::new(
                ErrorCode::ObjectChecksumMismatch,
                format!(
                    "delivered {delivered} bytes with checksum {computed:?}; record says {} bytes, {:?}",
                    record.size, record.object_checksum
                ),
            )
        };
        return Err(fail_and_close(writer, id, detail).await);
    }
    write_message(
        writer,
        &Message::EndOfStream {
            id,
            end: StreamEnd::ok(),
        },
    )
    .await?;
    Ok(())
}

// ------------------------------------------------------------------ PUT

struct PutParams {
    key: String,
    size: u64,
    content_type: Option<String>,
    user_metadata: BTreeMap<String, String>,
}

struct Holder {
    index: ShardIndex,
    device: DeviceId,
    owner: NodeId,
    connection: Connection,
    request_id: u32,
}

/// Choose k+m distinct active devices with room for a shard file, most
/// free space first, ties by device id (10.4, 10.5).
fn place(
    statuses: &[DeviceStatus],
    scheme: Scheme,
    shard_bytes: u64,
) -> Result<Vec<DeviceStatus>, Failure> {
    let mut eligible: Vec<&DeviceStatus> = statuses
        .iter()
        .filter(|d| d.state == DeviceState::Active && d.free_bytes >= shard_bytes)
        .collect();
    eligible.sort_by(|a, b| {
        b.free_bytes
            .cmp(&a.free_bytes)
            .then(a.device.cmp(&b.device))
    });
    if eligible.len() < scheme.total_shards() {
        return Err(error(
            ErrorCode::InsufficientDevices,
            format!(
                "{} active devices have {shard_bytes} bytes free; {} are needed",
                eligible.len(),
                scheme.total_shards()
            ),
        ));
    }
    Ok(eligible[..scheme.total_shards()]
        .iter()
        .map(|d| (*d).clone())
        .collect())
}

async fn abort_holders(holders: &mut [Holder], key_hash: KeyHash, version: VersionId) {
    for holder in holders.iter_mut() {
        // Best effort. Dropping the connection also drops any temporary on
        // the holder's side.
        let _ = holder
            .connection
            .request(Request::AbortShard {
                device: holder.device,
                key_hash,
                version,
                shard_index: holder.index.0,
            })
            .await;
    }
}

async fn put_object(
    node: &Arc<Node>,
    versions: &VersionGenerator,
    id: u32,
    reader: &mut Reader,
    writer: &mut Writer,
    params: PutParams,
) -> Result<(), Failure> {
    // The client streams the body right behind the request, so from here
    // on any refusal must also close the connection (fail_and_close).
    let prepared = prepare_put(node, versions, &params).await;
    let (scheme, record_template, mut holders) = match prepared {
        Ok(p) => p,
        Err(Failure::Error(detail)) => return Err(fail_and_close(writer, id, detail).await),
        Err(other) => return Err(other),
    };
    let key_hash = record_template.key_hash;
    let version = record_template.version;

    let outcome = stream_body_to_holders(reader, &mut holders, scheme, &record_template).await;
    let object_checksum = match outcome {
        Ok(checksum) => checksum,
        Err(Failure::Error(detail)) => {
            abort_holders(&mut holders, key_hash, version).await;
            return Err(fail_and_close(writer, id, detail).await);
        }
        Err(other) => {
            abort_holders(&mut holders, key_hash, version).await;
            return Err(other);
        }
    };

    let record = MetadataRecord {
        object_checksum,
        ..record_template
    };
    if let Err(f) = write_records(&mut holders, &record).await {
        abort_holders(&mut holders, key_hash, version).await;
        return match f {
            Failure::Error(detail) => Err(fail_and_close(writer, id, detail).await),
            other => Err(other),
        };
    }
    drop(holders);

    // Replace (9.2.4): the new version is durable everywhere; remove any
    // older versions of the key.
    let older = match lookup(node, key_hash)
        .await
        .and_then(|l| versions_of(&record.key, l))
    {
        Ok(versions) => versions,
        Err(Failure::Error(detail)) => return Err(fail_and_close(writer, id, detail).await),
        Err(other) => return Err(other),
    };
    for old in older.iter().filter(|r| r.version != version) {
        if let Err(f) = delete_version_everywhere(node, old).await {
            return match f {
                Failure::Error(detail) => Err(fail_and_close(writer, id, detail).await),
                other => Err(other),
            };
        }
    }
    respond(writer, id, Ok(Response::PutObject { version })).await
}

/// Everything before the first body byte is consumed: checks, placement,
/// and holders ready to receive.
async fn prepare_put(
    node: &Arc<Node>,
    versions: &VersionGenerator,
    params: &PutParams,
) -> Result<(Scheme, MetadataRecord, Vec<Holder>), Failure> {
    check_key(&params.key)?;
    if params.size > MAX_OBJECT_BYTES {
        return Err(error(
            ErrorCode::ObjectTooLarge,
            format!(
                "object of {} bytes exceeds the maximum of {MAX_OBJECT_BYTES}",
                params.size
            ),
        ));
    }
    let document = node.document();
    let scheme = document
        .scheme()
        .map_err(|e| error(ErrorCode::Internal, e.to_string()))?;
    let key_hash = hash_key(params.key.as_bytes());

    // An existing record under this hash with a different key is a
    // collision or corruption; refuse (9.1.6). Same key is fine: replace.
    let existing = lookup(node, key_hash).await?;
    versions_of(&params.key, existing)?;

    // Placement from current free space (10.3 to 10.5).
    let mut statuses: Vec<DeviceStatus> = Vec::new();
    for (target, response) in broadcast(node, Request::LocalStatus).await? {
        match response {
            Response::LocalStatus { devices, .. } => statuses.extend(devices),
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalStatus with {other:?}"),
                ))
            }
        }
    }
    let shard_bytes = if params.size == 0 {
        0
    } else {
        shard_file_length(scheme, document.block_size, params.size)
            .ok_or_else(|| error(ErrorCode::Internal, "cannot size shard file"))?
    };
    let chosen = place(&statuses, scheme, shard_bytes)?;
    let version = versions.next();

    let mut holders = Vec::with_capacity(chosen.len());
    let mut shards = Vec::with_capacity(chosen.len());
    for (i, status) in chosen.iter().enumerate() {
        let index = ShardIndex(i as u8);
        shards.push(ShardLocation {
            index: index.0,
            device: status.device,
        });
        let mut connection = connect_to(node, status.node).await?;
        let mut request_id = 0;
        if params.size > 0 {
            request_id = connection
                .send_request(Request::PutShard {
                    device: status.device,
                    key_hash,
                    version,
                    shard_index: index.0,
                    k: scheme.data_shards() as u8,
                    m: scheme.parity_shards() as u8,
                    block_length: document.block_size,
                    object_size: params.size,
                })
                .await
                .map_err(|e| remote_failure(status.node, e))?;
            match connection.read_response(request_id).await {
                Ok(Response::PutShardReady) => {}
                Ok(other) => {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!("{} answered PutShard with {other:?}", status.node),
                    ))
                }
                Err(e) => return Err(remote_failure(status.node, e)),
            }
        }
        holders.push(Holder {
            index,
            device: status.device,
            owner: status.node,
            connection,
            request_id,
        });
    }

    let record = MetadataRecord {
        format_version: RECORD_FORMAT_VERSION,
        system: SYSTEM_NAME.to_string(),
        bucket: "default".to_string(),
        key: params.key.clone(),
        key_hash,
        version,
        created: OffsetDateTime::now_utc(),
        size: params.size,
        object_checksum: BlockChecksum(0), // filled in after the body
        k: scheme.data_shards() as u8,
        m: scheme.parity_shards() as u8,
        block_size: document.block_size,
        shards,
        content_type: params.content_type.clone(),
        user_metadata: params.user_metadata.clone(),
    };
    Ok((scheme, record, holders))
}

/// Read the client's body stream, encode it stripe by stripe, fan the
/// blocks out to the holders, and finish every shard. Returns the
/// whole-object checksum.
async fn stream_body_to_holders(
    reader: &mut Reader,
    holders: &mut [Holder],
    scheme: Scheme,
    record: &MetadataRecord,
) -> Result<BlockChecksum, Failure> {
    let code = ReedSolomonCode::new(scheme);
    let stripe_size = scheme.data_shards() * record.block_size as usize;
    let mut hasher = Xxh3::new();
    let mut received: u64 = 0;
    let mut expected_sequence: u64 = 0;
    let mut stripe_buffer: Vec<u8> = Vec::with_capacity(stripe_size.min(1 << 26));
    let mut stripe_number: u64 = 0;

    loop {
        let message = read_message(reader).await?;
        match message {
            Message::Data { data, .. } => {
                if data.sequence != expected_sequence {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!(
                            "body chunk {} arrived when {expected_sequence} was expected",
                            data.sequence
                        ),
                    ));
                }
                expected_sequence += 1;
                if checksum_block(&data.bytes) != data.checksum {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!(
                            "body chunk {} failed its checksum in transit",
                            data.sequence
                        ),
                    ));
                }
                received += data.bytes.len() as u64;
                if received > record.size {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!("body exceeds the declared size of {} bytes", record.size),
                    ));
                }
                hasher.update(&data.bytes);
                let mut bytes: &[u8] = &data.bytes;
                while !bytes.is_empty() {
                    let room = stripe_size - stripe_buffer.len();
                    let take = room.min(bytes.len());
                    stripe_buffer.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                    if stripe_buffer.len() == stripe_size {
                        send_stripe(holders, &code, &stripe_buffer, record, stripe_number).await?;
                        stripe_number += 1;
                        stripe_buffer.clear();
                    }
                }
            }
            Message::EndOfStream { end, .. } => {
                if let Some(e) = end.error {
                    return Err(error(
                        ErrorCode::WriteFailed,
                        format!("client abandoned the upload: {}", e.message),
                    ));
                }
                if received != record.size {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!(
                            "body was {received} bytes but {} were declared",
                            record.size
                        ),
                    ));
                }
                if !stripe_buffer.is_empty() {
                    send_stripe(holders, &code, &stripe_buffer, record, stripe_number).await?;
                }
                break;
            }
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("expected body Data or EndOfStream, got {other:?}"),
                ))
            }
        }
    }

    let object_checksum = BlockChecksum(hasher.digest());
    if record.size > 0 {
        for holder in holders.iter_mut() {
            holder
                .connection
                .send_end(
                    holder.request_id,
                    StreamEnd {
                        error: None,
                        object_size: Some(record.size),
                        object_checksum: Some(object_checksum),
                    },
                )
                .await
                .map_err(|e| remote_failure(holder.owner, e))?;
        }
        for holder in holders.iter_mut() {
            match holder.connection.read_response(holder.request_id).await {
                Ok(Response::PutShardDone) => {}
                Ok(other) => {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!("{} ended PutShard with {other:?}", holder.owner),
                    ))
                }
                Err(e) => return Err(remote_failure(holder.owner, e)),
            }
        }
    }
    Ok(object_checksum)
}

async fn send_stripe(
    holders: &mut [Holder],
    code: &ReedSolomonCode,
    stripe: &[u8],
    record: &MetadataRecord,
    stripe_number: u64,
) -> Result<(), Failure> {
    let blocks = encode_stripe(code, stripe, record.block_size as usize)
        .map_err(|e| error(ErrorCode::Internal, format!("encode failed: {e}")))?;
    for (holder, block) in holders.iter_mut().zip(blocks) {
        debug_assert_eq!(holder.index, block.index);
        holder
            .connection
            .send_data(
                holder.request_id,
                DataFrame {
                    sequence: stripe_number,
                    checksum: block.checksum,
                    bytes: block.bytes,
                },
            )
            .await
            .map_err(|e| remote_failure(holder.owner, e))?;
    }
    Ok(())
}

async fn write_records(holders: &mut [Holder], record: &MetadataRecord) -> Result<(), Failure> {
    for holder in holders.iter_mut() {
        match holder
            .connection
            .request(Request::PutMeta {
                device: holder.device,
                record: record.clone(),
            })
            .await
        {
            Ok(Response::PutMeta) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{} answered PutMeta with {other:?}", holder.owner),
                ))
            }
            Err(e) => return Err(remote_failure(holder.owner, e)),
        }
    }
    Ok(())
}

// --------------------------------------------------------------- REPAIR

/// One shard as the repair sees it: a stream of blocks from its holder,
/// or nothing if the file could not be opened.
struct RepairSource {
    index: ShardIndex,
    device: DeviceId,
    owner: NodeId,
    /// `None` when the shard file could not be opened.
    stream: Option<(Connection, u32)>,
    condition: ShardCondition,
}

/// Rebuild every damaged or missing shard of a key's newest version
/// (SPEC 18.3, 18.4). Every intact shard is read in full and verified;
/// every stripe is decoded, checked, and re-encoded; each damaged shard
/// is written afresh through `PutShard` to the device the record names,
/// replacing the old file only once the whole object has verified against
/// the record's checksum. Fail-stop: an unreachable holder or more than m
/// damaged shards is an error and nothing is changed.
async fn repair_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    let record = newest_version(node, key).await?;
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let document = node.document();

    let mut shards: Vec<ShardRepair> = Vec::with_capacity(scheme.total_shards());
    if record.size == 0 {
        for shard in &record.shards {
            shards.push(ShardRepair {
                index: shard.index,
                device: shard.device,
                condition: ShardCondition::Intact,
                rewritten: false,
            });
        }
        return Ok(Response::RepairObject(RepairReport {
            key: key.to_string(),
            version: record.version,
            shards,
        }));
    }
    let geometry = shard_geometry(scheme, record.block_size, record.size).ok_or_else(|| {
        error(
            ErrorCode::RecordsInconsistent,
            "record has an impossible size",
        )
    })?;

    // Open a block stream from every holder. A holder that cannot open the
    // file is a damaged shard, not a failure; an unreachable holder is a
    // failure.
    let mut sources: Vec<RepairSource> = Vec::with_capacity(scheme.total_shards());
    for index in scheme.shard_indices() {
        let device = record
            .device_for(index)
            .expect("validated record lists every index");
        let owner = document.device(device).map(|d| d.node).ok_or_else(|| {
            Failure::Error(ErrorDetail {
                device: Some(device),
                key: Some(key.to_string()),
                version: Some(record.version),
                shard_index: Some(index.0),
                ..ErrorDetail::new(
                    ErrorCode::DeviceUnavailable,
                    format!("{device} is not in the cluster document"),
                )
            })
        })?;
        let mut connection = connect_to(node, owner).await?;
        let request_id = connection
            .send_request(Request::GetShard {
                device,
                key_hash: record.key_hash,
                version: record.version,
                shard_index: index.0,
                first_block: 0,
                block_count: geometry.block_count,
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
        let (stream, condition) = match connection.read_response(request_id).await {
            Ok(Response::GetShard { .. }) => {
                (Some((connection, request_id)), ShardCondition::Intact)
            }
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{owner} answered GetShard with {other:?}"),
                ))
            }
            // The holder answered, but could not serve the file: damaged.
            Err(ClientError::Remote(detail)) => (
                None,
                ShardCondition::Unreadable {
                    reason: detail.message,
                },
            ),
            Err(e) => return Err(remote_failure(owner, e)),
        };
        sources.push(RepairSource {
            index,
            device,
            owner,
            stream,
            condition,
        });
    }

    // Pass 1: read every stripe from every readable shard, verify, decode,
    // and re-encode, streaming the rebuilt blocks of damaged shards to
    // fresh files. Damaged shards are known fully only at the end of the
    // pass (a block may fail in the last stripe), so rebuilt blocks for
    // every shard that has shown any damage so far cannot be sent
    // incrementally without knowing the set up front. Instead this pass
    // records per-stripe faults and buffers nothing; pass 2 re-reads.
    let code = ReedSolomonCode::new(scheme);
    let all_indices = scheme.shard_indices();
    let stripe_size = scheme.data_shards() as u64 * record.block_size;
    let mut hasher = Xxh3::new();
    let mut corrupt_stripes: Vec<Vec<u64>> = vec![Vec::new(); scheme.total_shards()];
    for stripe in 0..geometry.block_count {
        let received = read_repair_stripe(&mut sources, stripe, key, &record).await?;
        let stripe_len = (record.size - stripe * stripe_size).min(stripe_size) as usize;
        let decoded = decode_stripe(&code, &all_indices, &received, stripe_len)
            .map_err(|e| error(ErrorCode::Internal, e.to_string()))?;
        let data = match decoded {
            DecodedStripe::Intact { data } => data,
            DecodedStripe::Repaired { data, faults } => {
                for fault in faults {
                    corrupt_stripes[fault.index.as_usize()].push(stripe);
                }
                data
            }
            DecodedStripe::Unrecoverable {
                faults,
                usable,
                needed,
            } => {
                return Err(Failure::Error(ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(
                        ErrorCode::BlockChecksumMismatch,
                        format!(
                            "stripe {stripe} has {usable} usable blocks of {needed} needed; {} damaged shard(s): {:?}",
                            faults.len(),
                            faults.iter().map(|f| f.index.0).collect::<Vec<u8>>()
                        ),
                    )
                }));
            }
        };
        hasher.update(&data);
    }
    finish_repair_streams(&mut sources, key, &record).await?;
    let computed = BlockChecksum(hasher.digest());
    if computed != record.object_checksum {
        return Err(Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            version: Some(record.version),
            ..ErrorDetail::new(
                ErrorCode::ObjectChecksumMismatch,
                format!(
                    "the intact shards decode to an object with checksum {computed:?}, but the record says {:?}; nothing rewritten",
                    record.object_checksum
                ),
            )
        }));
    }

    // Which shards need rewriting: unreadable ones, and readable ones
    // with any corrupt block.
    for source in sources.iter_mut() {
        let stripes = std::mem::take(&mut corrupt_stripes[source.index.as_usize()]);
        if source.stream.is_some() && !stripes.is_empty() {
            source.condition = ShardCondition::CorruptBlocks { stripes };
        }
    }
    let damaged: Vec<usize> = sources
        .iter()
        .enumerate()
        .filter(|(_, s)| s.condition != ShardCondition::Intact)
        .map(|(i, _)| i)
        .collect();
    if damaged.is_empty() {
        return Ok(Response::RepairObject(RepairReport {
            key: key.to_string(),
            version: record.version,
            shards: sources
                .iter()
                .map(|s| ShardRepair {
                    index: s.index.0,
                    device: s.device,
                    condition: ShardCondition::Intact,
                    rewritten: false,
                })
                .collect(),
        }));
    }

    // Pass 2: stream the intact shards again, re-encode each stripe, and
    // send the damaged shards' blocks to fresh files on their devices.
    // The old files are replaced only when each PutShard finishes.
    let mut writers: Vec<Holder> = Vec::with_capacity(damaged.len());
    for &i in &damaged {
        let source = &sources[i];
        let mut connection = connect_to(node, source.owner).await?;
        let request_id = connection
            .send_request(Request::PutShard {
                device: source.device,
                key_hash: record.key_hash,
                version: record.version,
                shard_index: source.index.0,
                k: scheme.data_shards() as u8,
                m: scheme.parity_shards() as u8,
                block_length: record.block_size,
                object_size: record.size,
            })
            .await
            .map_err(|e| remote_failure(source.owner, e))?;
        match connection.read_response(request_id).await {
            Ok(Response::PutShardReady) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{} answered PutShard with {other:?}", source.owner),
                ))
            }
            Err(e) => return Err(remote_failure(source.owner, e)),
        }
        writers.push(Holder {
            index: source.index,
            device: source.device,
            owner: source.owner,
            connection,
            request_id,
        });
    }
    let mut readers = reopen_intact_streams(node, &sources, &record, geometry.block_count).await?;
    for stripe in 0..geometry.block_count {
        let received = read_repair_stripe(&mut readers, stripe, key, &record).await?;
        let stripe_len = (record.size - stripe * stripe_size).min(stripe_size) as usize;
        let data = match decode_stripe(&code, &all_indices, &received, stripe_len)
            .map_err(|e| error(ErrorCode::Internal, e.to_string()))?
        {
            DecodedStripe::Intact { data } | DecodedStripe::Repaired { data, .. } => data,
            DecodedStripe::Unrecoverable { .. } => {
                return Err(error(
                    ErrorCode::BlockChecksumMismatch,
                    format!("stripe {stripe} became unrecoverable between passes"),
                ))
            }
        };
        let blocks = encode_stripe(&code, &data, record.block_size as usize)
            .map_err(|e| error(ErrorCode::Internal, format!("encode failed: {e}")))?;
        for writer in writers.iter_mut() {
            let block = &blocks[writer.index.as_usize()];
            writer
                .connection
                .send_data(
                    writer.request_id,
                    DataFrame {
                        sequence: stripe,
                        checksum: block.checksum,
                        bytes: block.bytes.clone(),
                    },
                )
                .await
                .map_err(|e| remote_failure(writer.owner, e))?;
        }
    }
    finish_repair_streams(&mut readers, key, &record).await?;
    for writer in writers.iter_mut() {
        writer
            .connection
            .send_end(
                writer.request_id,
                StreamEnd {
                    error: None,
                    object_size: Some(record.size),
                    object_checksum: Some(record.object_checksum),
                },
            )
            .await
            .map_err(|e| remote_failure(writer.owner, e))?;
    }
    for writer in writers.iter_mut() {
        match writer.connection.read_response(writer.request_id).await {
            Ok(Response::PutShardDone) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{} ended PutShard with {other:?}", writer.owner),
                ))
            }
            Err(e) => return Err(remote_failure(writer.owner, e)),
        }
    }

    let rewritten: Vec<ShardIndex> = writers.iter().map(|w| w.index).collect();
    Ok(Response::RepairObject(RepairReport {
        key: key.to_string(),
        version: record.version,
        shards: sources
            .iter()
            .map(|s| ShardRepair {
                index: s.index.0,
                device: s.device,
                condition: s.condition.clone(),
                rewritten: rewritten.contains(&s.index),
            })
            .collect(),
    }))
}

/// Read stripe `stripe` from every source that has a stream. Blocks are
/// returned unverified for the decoder, which treats mismatches as
/// erasures; a source with no stream contributes nothing.
async fn read_repair_stripe(
    sources: &mut [RepairSource],
    stripe: u64,
    key: &str,
    record: &MetadataRecord,
) -> Result<Vec<ShardBlock>, Failure> {
    let mut received = Vec::with_capacity(sources.len());
    for source in sources.iter_mut() {
        let Some((connection, request_id)) = source.stream.as_mut() else {
            continue;
        };
        match connection.read_stream_item(*request_id).await {
            Ok(StreamItem::Data(data)) if data.sequence == stripe => received.push(ShardBlock {
                index: source.index,
                bytes: data.bytes,
                checksum: data.checksum,
            }),
            Ok(StreamItem::Data(data)) => {
                return Err(Failure::Error(ErrorDetail {
                    node: Some(source.owner),
                    device: Some(source.device),
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(source.index.0),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(
                        ErrorCode::ProtocolViolation,
                        format!(
                            "holder sent block {} when {stripe} was expected",
                            data.sequence
                        ),
                    )
                }))
            }
            Ok(StreamItem::End(end)) => {
                // A holder whose file fails part way (a read error) ends
                // its stream early. Treat the shard as unreadable from
                // here on and let the decoder work without it.
                source.condition = ShardCondition::Unreadable {
                    reason: end
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "stream ended early".to_string()),
                };
                source.stream = None;
            }
            Err(e) => return Err(remote_failure(source.owner, e)),
        }
    }
    Ok(received)
}

/// Consume the clean end of every remaining stream.
async fn finish_repair_streams(
    sources: &mut [RepairSource],
    key: &str,
    record: &MetadataRecord,
) -> Result<(), Failure> {
    for source in sources.iter_mut() {
        let Some((connection, request_id)) = source.stream.as_mut() else {
            continue;
        };
        match connection.read_stream_item(*request_id).await {
            Ok(StreamItem::End(end)) if end.error.is_none() => {}
            other => {
                return Err(Failure::Error(ErrorDetail {
                    node: Some(source.owner),
                    device: Some(source.device),
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(source.index.0),
                    ..ErrorDetail::new(
                        ErrorCode::ProtocolViolation,
                        format!("holder did not end its stream cleanly: {other:?}"),
                    )
                }))
            }
        }
    }
    Ok(())
}

/// Fresh block streams for the shards found intact in pass 1.
async fn reopen_intact_streams(
    node: &Arc<Node>,
    sources: &[RepairSource],
    record: &MetadataRecord,
    block_count: u64,
) -> Result<Vec<RepairSource>, Failure> {
    let mut readers = Vec::new();
    for source in sources
        .iter()
        .filter(|s| s.condition == ShardCondition::Intact)
    {
        let mut connection = connect_to(node, source.owner).await?;
        let request_id = connection
            .send_request(Request::GetShard {
                device: source.device,
                key_hash: record.key_hash,
                version: record.version,
                shard_index: source.index.0,
                first_block: 0,
                block_count,
            })
            .await
            .map_err(|e| remote_failure(source.owner, e))?;
        match connection.read_response(request_id).await {
            Ok(Response::GetShard { .. }) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{} answered GetShard with {other:?}", source.owner),
                ))
            }
            Err(e) => return Err(remote_failure(source.owner, e)),
        }
        readers.push(RepairSource {
            index: source.index,
            device: source.device,
            owner: source.owner,
            stream: Some((connection, request_id)),
            condition: ShardCondition::Intact,
        });
    }
    Ok(readers)
}
