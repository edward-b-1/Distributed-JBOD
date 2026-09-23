//! The node-to-node operations (SPEC 19.1.3), served from this node's own
//! devices. Each handler answers one request on one connection; the
//! streaming ones also read or write `Data` frames for that request id.
//!
//! Every device call goes through `spawn_blocking` (4.4).

use std::sync::Arc;

use djbod_core::cluster::DeviceState;
use djbod_core::device::{Device, DeviceError, ShardWrite, WalkStep};
use djbod_core::erasure::{Scheme, ShardIndex};
use djbod_core::keyhash::KeyHash;
use djbod_core::layout::shard_file_name;
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::shardfile::{ShardFileError, ShardFileHeader};
use djbod_core::stripe::ShardBlock;
use djbod_core::version::VersionId;
use djbod_proto::message::{
    DataFrame, DeviceRecord, DeviceStatus, ErrorCode, ErrorDetail, KeyEntry, ListQuery,
    LocatedRecord, LookupCursor, Message, RecordCursor, Request, Response, ScrubItem, StreamEnd,
    MAX_LIST_PAGE_BYTES,
};

use crate::node::{Node, NodeError, ShardWriteKey};
use crate::server::{ConnectionEnd, Reader, Writer};
use djbod_client::wire::{read_message_within, write_message};

/// Serve one request. `Err` closes the connection; ordinary failures are
/// reported to the peer as `Response::Error` and return `Ok`.
pub async fn handle(
    node: &Arc<Node>,
    id: u32,
    request: Request,
    reader: &mut Reader,
    writer: &mut Writer,
) -> Result<(), ConnectionEnd> {
    let outcome: Result<(), Failure> = match request {
        Request::LocalStatus => respond(writer, id, local_status(node).await).await,
        Request::LocalLookup { key_hash, after } => {
            respond(writer, id, local_lookup(node, key_hash, after).await).await
        }
        Request::LocalList(query) => respond(writer, id, local_list(node, query).await).await,
        Request::LocalRecords { device, after } => {
            respond(writer, id, local_records(node, device, after).await).await
        }
        Request::LocalScrub {
            max_bytes_per_second,
        } => local_scrub(node, id, writer, max_bytes_per_second).await,
        Request::PutShard {
            device,
            key_hash,
            version,
            shard_index,
            k,
            m,
            block_length,
            object_size,
        } => {
            put_shard(
                node,
                id,
                reader,
                writer,
                PutShardParams {
                    device,
                    key_hash,
                    version,
                    shard_index,
                    k,
                    m,
                    block_length,
                    object_size,
                },
            )
            .await
        }
        Request::GetShard {
            device,
            key_hash,
            version,
            shard_index,
            first_block,
            block_count,
        } => {
            get_shard(
                node,
                id,
                writer,
                device,
                key_hash,
                version,
                shard_index,
                first_block,
                block_count,
            )
            .await
        }
        Request::PutMeta { device, record } => {
            respond(writer, id, put_meta(node, device, record).await).await
        }
        Request::GetMeta {
            device,
            key_hash,
            version,
            probe,
        } => {
            respond(
                writer,
                id,
                get_meta(node, device, key_hash, version, probe).await,
            )
            .await
        }
        Request::DeleteVersion {
            device,
            key_hash,
            version,
        } => {
            respond(
                writer,
                id,
                delete_version(node, device, key_hash, version).await,
            )
            .await
        }
        Request::AbortShard {
            device,
            key_hash,
            version,
            shard_index,
        } => {
            respond(
                writer,
                id,
                abort_shard(node, device, key_hash, version, shard_index).await,
            )
            .await
        }
        Request::GetClusterConfig => {
            respond(
                writer,
                id,
                Ok(Response::GetClusterConfig {
                    document: node.document(),
                }),
            )
            .await
        }
        Request::ApplyClusterConfig { document } => {
            let result = node
                .apply_document(document)
                .map(|_| Response::ApplyClusterConfig);
            respond(writer, id, result.map_err(Failure::from)).await
        }
        // Client-facing operations are dispatched to the coordinator by
        // the server before reaching here.
        other => {
            respond(
                writer,
                id,
                Err(Failure::Error(ErrorDetail::new(
                    ErrorCode::ProtocolViolation,
                    format!("{other:?} is a client operation, not a node-to-node one"),
                ))),
            )
            .await
        }
    };
    match outcome {
        Ok(()) => Ok(()),
        Err(Failure::Error(_)) => Ok(()), // already reported to the peer
        Err(Failure::Close(end)) => Err(end),
    }
}

/// How a handler failed: something to tell the peer, or a reason to close
/// the connection.
pub(crate) enum Failure {
    Error(ErrorDetail),
    Close(ConnectionEnd),
}

impl From<ErrorDetail> for Failure {
    fn from(detail: ErrorDetail) -> Failure {
        Failure::Error(detail)
    }
}

impl From<DeviceError> for Failure {
    fn from(e: DeviceError) -> Failure {
        Failure::Error(device_error_detail(e))
    }
}

impl From<NodeError> for Failure {
    fn from(e: NodeError) -> Failure {
        let code = match e {
            NodeError::NotNewer { .. } | NodeError::WrongCluster { .. } => {
                ErrorCode::DocumentVersionMismatch
            }
            NodeError::InvalidDocument(_) => ErrorCode::ProtocolViolation,
            _ => ErrorCode::Internal,
        };
        Failure::Error(ErrorDetail::new(code, e.to_string()))
    }
}

impl From<djbod_client::wire::WireError> for Failure {
    fn from(e: djbod_client::wire::WireError) -> Failure {
        Failure::Close(e.into())
    }
}

/// Write the response for `id`, or the error, to the peer.
pub(crate) async fn respond(
    writer: &mut Writer,
    id: u32,
    result: Result<Response, Failure>,
) -> Result<(), Failure> {
    let (response, outcome) = match result {
        Ok(response) => (response, Ok(())),
        Err(Failure::Error(detail)) => {
            (Response::Error(detail.clone()), Err(Failure::Error(detail)))
        }
        Err(Failure::Close(end)) => return Err(Failure::Close(end)),
    };
    write_message(writer, &Message::Response { id, response }).await?;
    outcome
}

pub(crate) fn device_error_detail(e: DeviceError) -> ErrorDetail {
    let code = match &e {
        DeviceError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            ErrorCode::NotFound
        }
        DeviceError::ShardFile(ShardFileError::Io(source))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            ErrorCode::NotFound
        }
        DeviceError::Io { .. } => ErrorCode::DeviceUnavailable,
        DeviceError::Record { .. } => ErrorCode::RecordsInconsistent,
        DeviceError::RecordExists { .. } => ErrorCode::WriteFailed,
        DeviceError::ShardFile(_) | DeviceError::BadObjectSize { .. } => ErrorCode::WriteFailed,
        _ => ErrorCode::Internal,
    };
    ErrorDetail::new(code, e.to_string())
}

fn with_device(detail: ErrorDetail, device: DeviceId) -> ErrorDetail {
    ErrorDetail {
        device: Some(device),
        ..detail
    }
}

/// The device named in a request, which must be one of ours.
fn own_device(node: &Node, id: DeviceId) -> Result<Arc<Device>, Failure> {
    node.device(id).ok_or_else(|| {
        Failure::Error(with_device(
            ErrorDetail::new(
                ErrorCode::DeviceUnavailable,
                format!("device {id:?} is not attached to node {:?}", node.id()),
            ),
            id,
        ))
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DeviceError> + Send + 'static,
) -> Result<T, Failure> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(Failure::from),
        Err(join) => Err(Failure::Error(ErrorDetail::new(
            ErrorCode::Internal,
            format!("disk task failed: {join}"),
        ))),
    }
}

// ------------------------------------------------------------- handlers

async fn local_status(node: &Arc<Node>) -> Result<Response, Failure> {
    let document = node.document();
    let mut devices = Vec::new();
    for device in node.devices() {
        let headroom = document.headroom;
        let state = document
            .device(device.id())
            .map(|d| d.state)
            .unwrap_or(DeviceState::Removed);
        let id = device.id();
        let space = blocking(move || device.space(headroom))
            .await
            .map_err(|f| match f {
                Failure::Error(d) => Failure::Error(with_device(d, id)),
                other => other,
            })?;
        devices.push(DeviceStatus {
            device: id,
            node: node.id(),
            state,
            label: document.device(id).and_then(|d| d.label.clone()),
            node_label: document.node(node.id()).and_then(|n| n.label.clone()),
            total_bytes: space.total_bytes,
            free_bytes: space.free_bytes,
        });
    }
    Ok(Response::LocalStatus {
        node: node.id(),
        document_version: document.version,
        tls_ready: node.tls().is_some(),
        build: Some(djbod_client::BUILD.to_string()),
        devices,
    })
}

/// Every record copy under `key_hash` on this node's devices, sorted by
/// version then device, in frame-sized pages (15.2.2): a record may be
/// as large as the document's user metadata limit allows, and a node
/// may hold many copies of one version.
async fn local_lookup(
    node: &Arc<Node>,
    key_hash: KeyHash,
    after: Option<LookupCursor>,
) -> Result<Response, Failure> {
    let mut records = Vec::new();
    for device in node.devices() {
        let id = device.id();
        let found = blocking(move || device.read_records(&key_hash))
            .await
            .map_err(|f| match f {
                Failure::Error(d) => Failure::Error(with_device(d, id)),
                other => other,
            })?;
        for record in found {
            records.push(LocatedRecord { device: id, record });
        }
    }
    records.sort_by(|a, b| {
        a.record
            .version
            .cmp(&b.record.version)
            .then(a.device.cmp(&b.device))
    });
    if let Some(after) = after {
        records.retain(|r| (r.record.version, r.device) > (after.version, after.device));
    }
    let mut page = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for located in records {
        let encoded = djbod_proto::codec::encode_cbor(&located)
            .map_err(|e| Failure::Error(ErrorDetail::new(ErrorCode::Internal, e.to_string())))?
            .len();
        if !page.is_empty() && bytes + encoded > MAX_LIST_PAGE_BYTES {
            truncated = true;
            break;
        }
        bytes += encoded;
        page.push(located);
    }
    Ok(Response::LocalLookup {
        records: page,
        truncated,
    })
}

/// Every readable record on one device, sorted by key then version, for
/// the drain (18.2.1). An unreadable record is logged and skipped; the
/// scrub is the tool that reports it.
async fn local_records(
    node: &Arc<Node>,
    device: DeviceId,
    after: Option<RecordCursor>,
) -> Result<Response, Failure> {
    let Some(device) = node.device(device) else {
        return Err(Failure::Error(ErrorDetail {
            device: Some(device),
            ..ErrorDetail::new(
                ErrorCode::DeviceUnavailable,
                format!("{device} is not a device of this node"),
            )
        }));
    };
    let id = device.id();
    // One page: as many records as fit in a frame with room to spare,
    // read from the cursor onwards and no further (SPEC 15.2.2).
    let (page, truncated, encode_failure) = blocking(move || {
        let mut page: Vec<DeviceRecord> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut encode_failure: Option<String> = None;
        device.walk_records_from(
            after.as_ref().map(|c| (&c.key_hash, c.version)),
            |record, shard_present| {
                let encoded = match djbod_proto::codec::encode_cbor(record) {
                    Ok(bytes) => bytes.len(),
                    Err(e) => {
                        encode_failure = Some(e.to_string());
                        return WalkStep::Stop;
                    }
                };
                if !page.is_empty() && bytes + encoded > MAX_LIST_PAGE_BYTES {
                    truncated = true;
                    return WalkStep::Stop;
                }
                bytes += encoded;
                page.push(DeviceRecord {
                    record: record.clone(),
                    shard_present,
                });
                WalkStep::Continue
            },
            |path, error| {
                tracing::warn!(path = %path.display(), %error, "unreadable record skipped");
            },
        )?;
        Ok((page, truncated, encode_failure))
    })
    .await
    .map_err(|f| match f {
        Failure::Error(d) => Failure::Error(with_device(d, id)),
        other => other,
    })?;
    if let Some(reason) = encode_failure {
        return Err(Failure::Error(ErrorDetail::new(
            ErrorCode::Internal,
            reason,
        )));
    }
    Ok(Response::LocalRecords {
        records: page,
        truncated,
    })
}

async fn local_list(node: &Arc<Node>, query: ListQuery) -> Result<Response, Failure> {
    // Collect from every device, drop duplicates (a node holding several
    // shards of one version has several copies of its record), sort,
    // then apply start_after and limit. See SPEC 15.2.1 for the scaling
    // question this leaves open.
    let mut entries: Vec<KeyEntry> = Vec::new();
    for device in node.devices() {
        let id = device.id();
        let prefix = query.prefix.clone();
        let found = blocking(move || {
            let mut out: Vec<KeyEntry> = Vec::new();
            device.walk_records(
                |record| {
                    let matches_prefix = match &prefix {
                        Some(p) => record.key.starts_with(p.as_str()),
                        None => true,
                    };
                    if matches_prefix {
                        out.push(KeyEntry {
                            key: record.key.clone(),
                            size: record.size,
                            version: record.version,
                        });
                    }
                },
                |path, error| {
                    tracing::warn!(path = %path.display(), %error, "unreadable record skipped");
                },
            )?;
            Ok(out)
        })
        .await
        .map_err(|f| match f {
            Failure::Error(d) => Failure::Error(with_device(d, id)),
            other => other,
        })?;
        entries.extend(found);
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key).then(a.version.cmp(&b.version)));
    entries.dedup_by(|a, b| a.key == b.key && a.version == b.version);
    if let Some(after) = &query.start_after {
        entries.retain(|e| e.key.as_str() > after.as_str());
    }
    let (entries, truncated) = page_of_keys(entries, query.limit);
    Ok(Response::LocalList { entries, truncated })
}

/// The first page of `entries`: at most `limit` of them, and at most
/// `MAX_LIST_PAGE_BYTES` of key text, so the response fits in a frame
/// (SPEC 15.2.1). The second value says whether any were left over.
pub fn page_of_keys(entries: Vec<KeyEntry>, limit: Option<u32>) -> (Vec<KeyEntry>, bool) {
    // A limit of zero would make an empty, truncated page, and a walk that
    // never advances; treat it as one.
    let limit = limit.map(|l| l.max(1));
    let mut page = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for entry in entries {
        let full = limit.is_some_and(|l| page.len() >= l as usize)
            || (!page.is_empty() && bytes + entry.key.len() > MAX_LIST_PAGE_BYTES);
        if full {
            truncated = true;
            break;
        }
        bytes += entry.key.len();
        page.push(entry);
    }
    (page, truncated)
}

struct PutShardParams {
    device: DeviceId,
    key_hash: KeyHash,
    version: VersionId,
    shard_index: u8,
    k: u8,
    m: u8,
    block_length: u64,
    object_size: u64,
}

/// `PutShard`: reserve and READY, then blocks in stripe order, then the
/// end-of-stream with object size and checksum, then DONE. Any error
/// after READY closes the connection, since the peer may still be
/// streaming; the temporary file is removed by dropping the write.
async fn put_shard(
    node: &Arc<Node>,
    id: u32,
    reader: &mut Reader,
    writer: &mut Writer,
    params: PutShardParams,
) -> Result<(), Failure> {
    let device = match own_device(node, params.device) {
        Ok(device) => device,
        Err(f) => return respond(writer, id, Err(f)).await,
    };
    let document = node.document();
    if document.device(params.device).map(|d| d.state) != Some(DeviceState::Active) {
        let detail = with_device(
            ErrorDetail::new(
                ErrorCode::WriteFailed,
                format!(
                    "device {:?} is not active and accepts no new shards",
                    params.device
                ),
            ),
            params.device,
        );
        return respond(writer, id, Err(Failure::Error(detail))).await;
    }
    let scheme = match Scheme::new(params.k, params.m) {
        Ok(scheme) => scheme,
        Err(e) => {
            return respond(
                writer,
                id,
                Err(Failure::Error(ErrorDetail::new(
                    ErrorCode::ProtocolViolation,
                    e.to_string(),
                ))),
            )
            .await
        }
    };
    let shard_index = ShardIndex(params.shard_index);
    if !scheme.contains(shard_index) {
        return respond(
            writer,
            id,
            Err(Failure::Error(ErrorDetail::new(
                ErrorCode::ProtocolViolation,
                format!("{shard_index} is outside scheme {}+{}", params.k, params.m),
            ))),
        )
        .await;
    }
    // One writer per shard per device (20.1.2.1). The guard is held until
    // this function returns, by success or by any failure path.
    let Some(_write_guard) = node.begin_shard_write(ShardWriteKey {
        device: params.device,
        key_hash: params.key_hash,
        version: params.version,
        shard_index: params.shard_index,
    }) else {
        let detail = with_device(
            ErrorDetail {
                version: Some(params.version),
                shard_index: Some(params.shard_index),
                ..ErrorDetail::new(
                    ErrorCode::WriteFailed,
                    "this shard is already being written on this device by another request"
                        .to_string(),
                )
            },
            params.device,
        );
        return respond(writer, id, Err(Failure::Error(detail))).await;
    };
    let header = ShardFileHeader {
        scheme,
        shard_index,
        block_length: params.block_length,
        key_hash: params.key_hash,
        version_id: params.version,
    };
    let key_hash = params.key_hash;
    let object_size = params.object_size;
    let begun = blocking(move || device.begin_shard(&key_hash, header, object_size)).await;
    let mut write: ShardWrite = match begun {
        Ok(write) => write,
        Err(Failure::Error(d)) => {
            return respond(
                writer,
                id,
                Err(Failure::Error(with_device(d, params.device))),
            )
            .await
        }
        Err(other) => return Err(other),
    };
    respond(writer, id, Ok(Response::PutShardReady)).await?;

    let mut expected_stripe: u64 = 0;
    let idle = node.stream_idle_timeout();
    loop {
        // A sender that stops mid-stream (a crashed coordinator, a client
        // that went away) must not leave this write open forever; the
        // dropped `write` removes the temporary (10.12).
        let message = read_message_within(reader, idle).await?;
        match message {
            Message::Data { id: got, data } if got == id => {
                if data.sequence != expected_stripe {
                    return Err(stream_violation(
                        writer,
                        id,
                        format!(
                            "stripe {} arrived when stripe {expected_stripe} was expected",
                            data.sequence
                        ),
                    )
                    .await);
                }
                let block = ShardBlock {
                    index: shard_index,
                    bytes: data.bytes,
                    checksum: data.checksum,
                };
                let appended = tokio::task::spawn_blocking(move || {
                    let result = write.append_block(&block);
                    (write, result)
                })
                .await;
                let (returned, result) = match appended {
                    Ok(pair) => pair,
                    Err(join) => {
                        return Err(stream_violation(
                            writer,
                            id,
                            format!("disk task failed: {join}"),
                        )
                        .await)
                    }
                };
                write = returned;
                if let Err(e) = result {
                    // The write is dropped on return, removing the temporary.
                    return Err(stream_violation(writer, id, device_error_detail(e).message).await);
                }
                expected_stripe += 1;
            }
            Message::EndOfStream { id: got, end } if got == id => {
                if let Some(error) = end.error {
                    // The sender gave up; the temporary goes with the write.
                    drop(write);
                    return respond(
                        writer,
                        id,
                        Err(Failure::Error(ErrorDetail::new(
                            ErrorCode::WriteFailed,
                            format!("sender abandoned the shard: {}", error.message),
                        ))),
                    )
                    .await;
                }
                let (Some(size), Some(checksum)) = (end.object_size, end.object_checksum) else {
                    return Err(stream_violation(
                        writer,
                        id,
                        "end of PutShard stream lacks object size or checksum".to_string(),
                    )
                    .await);
                };
                if size != params.object_size {
                    return Err(stream_violation(
                        writer,
                        id,
                        format!(
                            "object size {size} at end of stream differs from {} in the request",
                            params.object_size
                        ),
                    )
                    .await);
                }
                let finished = blocking(move || write.finish(size, checksum)).await;
                return match finished {
                    Ok(_footer) => respond(writer, id, Ok(Response::PutShardDone)).await,
                    Err(Failure::Error(d)) => {
                        respond(
                            writer,
                            id,
                            Err(Failure::Error(with_device(d, params.device))),
                        )
                        .await
                    }
                    Err(other) => Err(other),
                };
            }
            other => {
                return Err(stream_violation(
                    writer,
                    id,
                    format!("expected Data or EndOfStream for request {id}, got {other:?}"),
                )
                .await)
            }
        }
    }
}

/// Report a mid-stream failure and close the connection.
pub(crate) async fn stream_violation(writer: &mut Writer, id: u32, message: String) -> Failure {
    let detail = ErrorDetail::new(ErrorCode::ProtocolViolation, message.clone());
    let _ = write_message(
        writer,
        &Message::Response {
            id,
            response: Response::Error(detail),
        },
    )
    .await;
    Failure::Close(ConnectionEnd::ProtocolViolation(message))
}

#[allow(clippy::too_many_arguments)]
async fn get_shard(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    device_id: DeviceId,
    key_hash: KeyHash,
    version: VersionId,
    shard_index: u8,
    first_block: u64,
    block_count: u64,
) -> Result<(), Failure> {
    let device = match own_device(node, device_id) {
        Ok(device) => device,
        Err(f) => return respond(writer, id, Err(f)).await,
    };
    let index = ShardIndex(shard_index);
    let opened = blocking(move || device.open_shard(&key_hash, &version, index)).await;
    let reader = match opened {
        Ok(reader) => Arc::new(reader),
        Err(Failure::Error(d)) => {
            return respond(writer, id, Err(Failure::Error(with_device(d, device_id)))).await
        }
        Err(other) => return Err(other),
    };
    let available = reader.block_count();
    if first_block
        .checked_add(block_count)
        .is_none_or(|end| end > available)
    {
        return respond(
            writer,
            id,
            Err(Failure::Error(ErrorDetail::new(
                ErrorCode::ProtocolViolation,
                format!("blocks {first_block}..{first_block}+{block_count} requested but the shard has {available}"),
            ))),
        )
        .await;
    }
    respond(writer, id, Ok(Response::GetShard { block_count })).await?;
    for block_number in first_block..first_block + block_count {
        let shard = reader.clone();
        let read = blocking(move || Ok(shard.read_block(block_number)?)).await;
        match read {
            Ok(block) => {
                write_message(
                    writer,
                    &Message::Data {
                        id,
                        data: DataFrame {
                            sequence: block_number,
                            checksum: block.checksum,
                            bytes: block.bytes,
                        },
                    },
                )
                .await?;
            }
            Err(Failure::Error(d)) => {
                let detail = ErrorDetail {
                    device: Some(device_id),
                    version: Some(version),
                    shard_index: Some(shard_index),
                    stripe: Some(block_number),
                    ..d
                };
                write_message(
                    writer,
                    &Message::EndOfStream {
                        id,
                        end: StreamEnd::failed(detail.clone()),
                    },
                )
                .await?;
                return Err(Failure::Error(detail));
            }
            Err(other) => return Err(other),
        }
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

async fn put_meta(
    node: &Arc<Node>,
    device_id: DeviceId,
    record: MetadataRecord,
) -> Result<Response, Failure> {
    let device = own_device(node, device_id)?;
    blocking(move || device.write_record(&record))
        .await
        .map_err(|f| match f {
            Failure::Error(d) => Failure::Error(with_device(d, device_id)),
            other => other,
        })?;
    Ok(Response::PutMeta)
}

async fn get_meta(
    node: &Arc<Node>,
    device_id: DeviceId,
    key_hash: KeyHash,
    version: VersionId,
    probe: bool,
) -> Result<Response, Failure> {
    let device = own_device(node, device_id)?;
    let result = blocking(move || {
        let record = match device.read_record(&key_hash, &version) {
            Ok(record) => Some(record),
            Err(DeviceError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        let shard_present = if probe {
            let present = match &record {
                Some(record) => match record.shard_on(device_id) {
                    Some(index) => device
                        .object_directory(&key_hash)
                        .join(shard_file_name(&version, index))
                        .is_file(),
                    None => false,
                },
                None => false,
            };
            Some(present)
        } else {
            None
        };
        Ok((record, shard_present))
    })
    .await
    .map_err(|f| match f {
        Failure::Error(d) => Failure::Error(with_device(d, device_id)),
        other => other,
    })?;
    Ok(Response::GetMeta {
        record: result.0,
        shard_present: result.1,
    })
}

async fn delete_version(
    node: &Arc<Node>,
    device_id: DeviceId,
    key_hash: KeyHash,
    version: VersionId,
) -> Result<Response, Failure> {
    let device = own_device(node, device_id)?;
    blocking(move || device.delete_version(&key_hash, &version))
        .await
        .map_err(|f| match f {
            Failure::Error(d) => Failure::Error(with_device(d, device_id)),
            other => other,
        })?;
    Ok(Response::DeleteVersion)
}

async fn abort_shard(
    node: &Arc<Node>,
    device_id: DeviceId,
    key_hash: KeyHash,
    version: VersionId,
    shard_index: u8,
) -> Result<Response, Failure> {
    let device = own_device(node, device_id)?;
    blocking(move || {
        device.remove_temporary_shard(&key_hash, &version, ShardIndex(shard_index))?;
        device.delete_version(&key_hash, &version)
    })
    .await
    .map_err(|f| match f {
        Failure::Error(d) => Failure::Error(with_device(d, device_id)),
        other => other,
    })?;
    Ok(Response::AbortShard)
}

/// `LocalScrub`: run the local scrub engine over every device, streaming
/// each finding as a CBOR `ScrubItem` frame as it is made, then a summary
/// per device, then end-of-stream. The engine runs on a blocking thread
/// and hands findings over a channel so the stream is written as they
/// arise rather than at the end.
async fn local_scrub(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    max_bytes_per_second: Option<u64>,
) -> Result<(), Failure> {
    use djbod_core::scrub::{scrub_device, ScrubOptions};
    respond(writer, id, Ok(Response::LocalScrubStarted)).await?;
    let options = ScrubOptions {
        max_bytes_per_second,
        temporary_max_age: std::time::Duration::from_secs(node.config().temporary_max_age_secs),
    };
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<ScrubItem>(64);
    let devices = node.devices();
    let engine = tokio::task::spawn_blocking(move || -> Result<(), DeviceError> {
        for device in devices {
            let device_id = device.id();
            let sender_for_findings = sender.clone();
            let mut on_finding = |finding: &djbod_core::scrub::Finding| {
                let _ = sender_for_findings.blocking_send(ScrubItem::Finding {
                    device: device_id,
                    finding: finding.clone(),
                });
            };
            let summary = scrub_device(&device, &options, &mut on_finding)?;
            let _ = sender.blocking_send(ScrubItem::Summary {
                device: device_id,
                summary,
            });
        }
        Ok(())
    });
    let mut sequence: u64 = 0;
    while let Some(item) = receiver.recv().await {
        let bytes = djbod_proto::codec::encode_cbor(&item)
            .map_err(|e| Failure::Error(ErrorDetail::new(ErrorCode::Internal, e.to_string())))?;
        write_message(
            writer,
            &Message::Data {
                id,
                data: DataFrame {
                    sequence,
                    checksum: djbod_core::checksum::checksum_block(&bytes),
                    bytes,
                },
            },
        )
        .await?;
        sequence += 1;
    }
    let end = match engine.await {
        Ok(Ok(())) => StreamEnd::ok(),
        Ok(Err(e)) => StreamEnd::failed(device_error_detail(e)),
        Err(join) => StreamEnd::failed(ErrorDetail::new(
            ErrorCode::Internal,
            format!("scrub task failed: {join}"),
        )),
    };
    write_message(writer, &Message::EndOfStream { id, end }).await?;
    Ok(())
}
