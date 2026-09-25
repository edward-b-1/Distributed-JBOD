//! The client-facing operations (SPEC 19.1.3), served by the node a
//! client connects to. The coordinator does everything by sending
//! node-to-node operations to the nodes in the cluster document, including
//! itself over the loopback interface, so there is one code path whether
//! the cluster has one node or twenty (4.1).
//!
//! Fail-stop (16): any node that does not answer, any device that cannot
//! be reached, any checksum that does not match, fails the request with
//! an error carrying the fields of 16.2.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;

use time::OffsetDateTime;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::Xxh3;

use djbod_core::checksum::{checksum_block, BlockChecksum};
use djbod_core::cluster::{ClusterDocument, DeviceState, NodeId};
use djbod_core::erasure::{ReedSolomonCode, Scheme, ShardIndex};
use djbod_core::keyhash::{hash_key, KeyHash};
use djbod_core::record::{
    DeviceId, MetadataRecord, ShardLocation, MAX_CONTENT_TYPE_BYTES, RECORD_FORMAT_VERSION,
    SYSTEM_NAME,
};
use djbod_core::shardfile::{shard_file_length, shard_geometry};
use djbod_core::stripe::{decode_stripe, encode_stripe, DecodedStripe, FaultKind, ShardBlock};
use djbod_core::version::VersionId;
use djbod_proto::message::{
    ClusterFinding, DataFrame, DeviceContents, DeviceExposure, DeviceRecord, DeviceStatus,
    DrainEvent, ErrorCode, ErrorDetail, KeyEntry, ListQuery, LocatedRecord, LookupCursor, Message,
    MissingRecordCopy, NodeStatus, Reconstruction, RecordCopyFault, RecordCursor, RepairReport,
    Request, Response, ScrubEvent, ScrubItem, ShardAvailability, ShardCondition, ShardRepair,
    StreamEnd, UnavailableDevice,
};

use crate::local_ops::{respond, Failure};
use crate::node::Node;
use crate::server::{our_hello, ConnectionEnd, Reader, Writer};
use crate::ulid::VersionGenerator;
use djbod_client::connection::{Connection, ConnectionError, StreamItem};
use djbod_client::wire::{read_message_within, write_message};

pub fn is_client_operation(request: &Request) -> bool {
    matches!(
        request,
        Request::Status
            | Request::DeviceContents { .. }
            | Request::PutObject { .. }
            | Request::GetObject { .. }
            | Request::HeadObject { .. }
            | Request::DeleteObject { .. }
            | Request::ListKeys(_)
            | Request::RepairObject { .. }
            | Request::MoveShard { .. }
            | Request::Scrub { .. }
            | Request::Drain { .. }
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
        Request::DeviceContents { device } => {
            respond(writer, id, device_contents(node, device).await).await
        }
        Request::HeadObject { key } => respond(writer, id, head_object(node, &key).await).await,
        Request::DeleteObject { key } => respond(writer, id, delete_object(node, &key).await).await,
        Request::ListKeys(query) => respond(writer, id, list_keys(node, query).await).await,
        Request::RepairObject { key } => respond(writer, id, repair_object(node, &key).await).await,
        Request::MoveShard {
            key,
            shard_index,
            target,
        } => {
            respond(
                writer,
                id,
                move_shard(node, &key, shard_index, target).await,
            )
            .await
        }
        Request::Scrub {
            max_bytes_per_second,
            repair,
        } => scrub(node, id, writer, max_bytes_per_second, repair).await,
        Request::Drain { device, partial } => drain(node, id, writer, device, partial).await,
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
    let connector = node
        .connector()
        .map_err(|e| error(ErrorCode::TlsRequired, e.to_string()))?;
    Connection::connect_with(&connector, address, our_hello(node))
        .await
        .map_err(|e| match e {
            // The peer answered and refused our Hello: it holds a different
            // document version (6.2.7) or is otherwise not our peer. Keep
            // its own account of why.
            ConnectionError::Remote(detail) => Failure::Error(ErrorDetail {
                node: Some(target),
                ..detail
            }),
            other => Failure::Error(ErrorDetail {
                node: Some(target),
                ..ErrorDetail::new(
                    ErrorCode::NodeUnreachable,
                    format!("cannot reach {target} at {address}: {other}"),
                )
            }),
        })
}

fn remote_failure(target: NodeId, e: ConnectionError) -> Failure {
    match e {
        ConnectionError::Remote(detail) => Failure::Error(ErrorDetail {
            node: detail.node.or(Some(target)),
            ..detail
        }),
        ConnectionError::StreamFailed(detail) => Failure::Error(ErrorDetail {
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
/// `request` to every node in the document, failing on the first node
/// that cannot answer (16.1).
async fn broadcast(node: &Arc<Node>, request: Request) -> Result<Vec<(NodeId, Response)>, Failure> {
    let mut responses = Vec::new();
    for (target, answer) in broadcast_each(node, request).await? {
        responses.push((target, answer?));
    }
    Ok(responses)
}

/// `request` to every node in the document, with each node's answer or
/// its failure, in node order, for an operation that goes on without a
/// node it cannot reach: `Status` (19.1.3, 5.6). Only a failure of the
/// machinery itself is an error here.
async fn broadcast_each(
    node: &Arc<Node>,
    request: Request,
) -> Result<Vec<(NodeId, Result<Response, Failure>)>, Failure> {
    let document = node.document();
    let mut tasks = JoinSet::new();
    for entry in &document.nodes {
        let target = entry.id;
        let node = node.clone();
        let request = request.clone();
        tasks.spawn(async move {
            let answer = async {
                let mut connection = connect_to(&node, target).await?;
                connection
                    .request(request)
                    .await
                    .map_err(|e| remote_failure(target, e))
            }
            .await;
            (target, answer)
        });
    }
    let mut answers = Vec::with_capacity(document.nodes.len());
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(pair) => answers.push(pair),
            Err(e) => {
                return Err(error(
                    ErrorCode::Internal,
                    format!("broadcast task failed: {e}"),
                ))
            }
        }
    }
    answers.sort_by_key(|(id, _)| *id);
    Ok(answers)
}

// ------------------------------------------------------------- lookups

/// What a broadcast lookup found on the devices it could reach.
struct Lookup {
    records: Vec<LocatedRecord>,
    /// The devices whose copies could not be looked for, and why: on a
    /// node that could not be reached, unreadable by their node (5.6),
    /// or removed from the cluster (18.2.1). The code is the one a
    /// request naming the device would be refused with.
    unread: BTreeMap<DeviceId, (ErrorCode, String)>,
    /// The nodes that could not be reached, for the operations that
    /// must reach every node (13.3, 16.1).
    unreachable: Vec<ErrorDetail>,
}

/// The broadcast lookup of section 13 as a read uses it: every record
/// copy for a key hash from every node that answers, asked concurrently
/// and each read to the end of its pages (15.2.2), and the devices whose
/// copies could not arrive, so that the read can tell a device that is
/// out from a copy that is gone (9.4.4).
async fn lookup_reachable(node: &Arc<Node>, key_hash: KeyHash) -> Result<Lookup, Failure> {
    let document = node.document();
    let mut tasks = JoinSet::new();
    for entry in &document.nodes {
        let target = entry.id;
        let node = node.clone();
        tasks.spawn(async move { (target, lookup_on(&node, target, key_hash).await) });
    }
    let mut found = Lookup {
        records: Vec::new(),
        unread: BTreeMap::new(),
        unreachable: Vec::new(),
    };
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((target, Ok((records, unread)))) => {
                found.records.extend(records);
                for device in unread {
                    found.unread.insert(
                        device,
                        (
                            ErrorCode::DeviceUnavailable,
                            format!("{device} is unavailable on {target} (5.6)"),
                        ),
                    );
                }
            }
            Ok((target, Err(Failure::Error(detail))))
                if detail.code == ErrorCode::NodeUnreachable =>
            {
                for device in document.devices.iter().filter(|d| d.node == target) {
                    found.unread.insert(
                        device.id,
                        (ErrorCode::NodeUnreachable, detail.message.clone()),
                    );
                }
                found.unreachable.push(detail);
            }
            Ok((_, Err(f))) => return Err(f),
            Err(e) => {
                return Err(error(
                    ErrorCode::Internal,
                    format!("lookup task failed: {e}"),
                ))
            }
        }
    }
    for device in &document.devices {
        if device.state == DeviceState::Removed {
            found.unread.entry(device.id).or_insert((
                ErrorCode::DeviceUnavailable,
                format!("{} was removed from the cluster (18.2.1)", device.id),
            ));
        }
    }
    found.records.sort_by(|a, b| {
        a.record
            .version
            .cmp(&b.record.version)
            .then(a.device.cmp(&b.device))
    });
    Ok(found)
}

/// The broadcast lookup as every operation but a read uses it (13.3):
/// a node that cannot be reached fails it, and a device its node cannot
/// read contributes nothing, which the copy count then shows (9.4.4).
async fn lookup(node: &Arc<Node>, key_hash: KeyHash) -> Result<Vec<LocatedRecord>, Failure> {
    let found = lookup_reachable(node, key_hash).await?;
    if let Some(detail) = found.unreachable.into_iter().next() {
        return Err(Failure::Error(detail));
    }
    Ok(found.records)
}

/// One node's record copies under `key_hash`, page by page, and the
/// devices it could not read (5.6), which it names on every page.
async fn lookup_on(
    node: &Arc<Node>,
    target: NodeId,
    key_hash: KeyHash,
) -> Result<(Vec<LocatedRecord>, Vec<DeviceId>), Failure> {
    let mut connection = connect_to(node, target).await?;
    let mut all = Vec::new();
    let mut after: Option<LookupCursor> = None;
    loop {
        let answer = connection
            .request(Request::LocalLookup {
                key_hash,
                after: after.clone(),
            })
            .await
            .map_err(|e| remote_failure(target, e))?;
        match answer {
            Response::LocalLookup {
                records,
                truncated,
                unread,
            } => {
                after = records.last().map(|r| LookupCursor {
                    version: r.record.version,
                    device: r.device,
                });
                all.extend(records);
                if !truncated || after.is_none() {
                    return Ok((all, unread));
                }
            }
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalLookup with {other:?}"),
                ))
            }
        }
    }
}

/// One version as the copies found describe it (9.4.4, 18.8.1): the
/// record at its highest placement revision, the devices it lists whose
/// copy of that revision did not arrive, and how many copies vouch for
/// the body, which counts lower-revision copies that describe the same
/// body, as repair counts them (18.4.2).
struct TrustedVersion {
    record: MetadataRecord,
    /// Faults `Missing` or `Stale` only: whether a missing copy's device
    /// could have been consulted is the caller's knowledge (`Lookup`).
    missing: Vec<MissingRecordCopy>,
    vouching: usize,
}

/// Group record copies by version, newest first, applying 9.4.4 as
/// every operation but a read does: within a version, take the highest
/// placement revision present, require every copy the record lists to
/// have arrived, all agreeing, and ignore lower-revision copies, which
/// are the leftovers of a re-placement (18.8.1).
fn versions_of(key: &str, located: Vec<LocatedRecord>) -> Result<Vec<MetadataRecord>, Failure> {
    versions_of_excluding(key, located, &BTreeSet::new())
}

/// `versions_of` for the cross-node scrub (20.1.2.2). `unread` names
/// the devices whose record streams failed as unavailable (5.6) during
/// this run, so none of their copies could arrive: a copy missing from
/// one of them is not evidence about the version, only about the device,
/// which the node's own scrub has already reported once. This decides
/// what the report says and nothing else; no action is taken on it.
fn versions_of_excluding(
    key: &str,
    located: Vec<LocatedRecord>,
    unread: &BTreeSet<DeviceId>,
) -> Result<Vec<MetadataRecord>, Failure> {
    let mut versions = Vec::new();
    for trusted in trusted_versions(key, located)? {
        let record = trusted.record;
        let unread_copies = trusted
            .missing
            .iter()
            .filter(|m| unread.contains(&m.device))
            .count();
        if unread_copies < trusted.missing.len() {
            let listed = record.k as usize + record.m as usize;
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(record.version),
                ..ErrorDetail::new(
                    ErrorCode::RecordsInconsistent,
                    format!(
                        "{} record copies found, {} expected (revision {})",
                        listed - trusted.missing.len(),
                        listed - unread_copies,
                        record.revision
                    ),
                )
            }));
        }
        versions.push(record);
    }
    Ok(versions)
}

/// The checks every operation makes of a version's copies (9.4.4): each
/// names the requested key (9.1.6), the copies at the highest revision
/// agree and each comes from a device the record lists. What a missing
/// copy means is left to the caller. Newest version first.
fn trusted_versions(
    key: &str,
    located: Vec<LocatedRecord>,
) -> Result<Vec<TrustedVersion>, Failure> {
    let mut by_version: BTreeMap<VersionId, Vec<LocatedRecord>> = BTreeMap::new();
    for item in located {
        by_version
            .entry(item.record.version)
            .or_default()
            .push(item);
    }
    let mut versions = Vec::with_capacity(by_version.len());
    for (version, copies) in by_version.into_iter().rev() {
        for copy in &copies {
            if copy.record.key != key {
                return Err(Failure::Error(ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(version),
                    device: Some(copy.device),
                    ..ErrorDetail::new(
                        ErrorCode::KeyMismatch,
                        format!(
                            "record under this key hash names key {:?}, not {key:?}: hash collision or corruption",
                            copy.record.key
                        ),
                    )
                }));
            }
        }
        let current_revision = copies
            .iter()
            .map(|c| c.record.revision)
            .max()
            .expect("non-empty");
        let current: Vec<&LocatedRecord> = copies
            .iter()
            .filter(|c| c.record.revision == current_revision)
            .collect();
        let first = &current[0].record;
        for copy in &current {
            if copy.record != *first {
                return Err(Failure::Error(ErrorDetail {
                    key: Some(key.to_string()),
                    version: Some(version),
                    device: Some(copy.device),
                    ..ErrorDetail::new(
                        ErrorCode::RecordsInconsistent,
                        format!("record copies at revision {current_revision} disagree"),
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
        let missing = first
            .shards
            .iter()
            .filter(|shard| !current.iter().any(|c| c.device == shard.device))
            .map(|shard| {
                let stale = copies
                    .iter()
                    .filter(|c| c.device == shard.device)
                    .map(|c| c.record.revision)
                    .max();
                MissingRecordCopy {
                    device: shard.device,
                    fault: match stale {
                        Some(revision) => RecordCopyFault::Stale { revision },
                        None => RecordCopyFault::Missing,
                    },
                }
            })
            .collect();
        let vouching = copies
            .iter()
            .filter(|c| c.record.revision == current_revision || first.same_body(&c.record))
            .count();
        versions.push(TrustedVersion {
            record: first.clone(),
            missing,
            vouching,
        });
    }
    Ok(versions)
}

/// Copies of `version` at a lower revision than `current`, on devices the
/// current record no longer lists: stale leftovers of a re-placement.
fn stale_copies(current: &MetadataRecord, located: &[LocatedRecord]) -> Vec<(DeviceId, u64)> {
    located
        .iter()
        .filter(|c| {
            c.record.version == current.version
                && c.record.revision < current.revision
                && current.shard_on(c.device).is_none()
        })
        .map(|c| (c.device, c.record.revision))
        .collect()
}

/// The key sanity check of 9.1.5, against the limit in the cluster
/// document.
fn check_key(node: &Node, key: &str) -> Result<(), Failure> {
    if key.is_empty() {
        return Err(error(ErrorCode::ProtocolViolation, "key is empty"));
    }
    let limit = node.document().max_key_bytes;
    if key.len() as u64 > limit {
        let shown: String = key.chars().take(64).collect();
        return Err(Failure::Error(ErrorDetail {
            key: Some(shown + "..."),
            ..ErrorDetail::new(
                ErrorCode::KeyTooLong,
                format!(
                    "key is {} bytes; the cluster's limit is {limit} (max_key_bytes)",
                    key.len()
                ),
            )
        }));
    }
    Ok(())
}

/// The newest version of `key`, or NotFound, for the operations that
/// must reach every copy (13.3).
async fn newest_version(node: &Arc<Node>, key: &str) -> Result<MetadataRecord, Failure> {
    check_key(node, key)?;
    let located = lookup(node, hash_key(key.as_bytes())).await?;
    let versions = versions_of(key, located)?;
    versions.into_iter().next().ok_or_else(|| not_found(key))
}

fn not_found(key: &str) -> Failure {
    Failure::Error(ErrorDetail {
        key: Some(key.to_string()),
        ..ErrorDetail::new(ErrorCode::NotFound, format!("no object under key {key:?}"))
    })
}

/// The newest version of `key` as a read trusts it (9.4.4 as amended by
/// 18.4.2): the copies that arrived must agree, and at least k must
/// vouch for the body; every listed copy that did not arrive is returned
/// beside the record, classified by what is known of its device, for
/// the client to hear about (11.7). Nothing is written.
async fn newest_readable_version(
    node: &Arc<Node>,
    key: &str,
) -> Result<(MetadataRecord, Vec<MissingRecordCopy>), Failure> {
    check_key(node, key)?;
    let found = lookup_reachable(node, hash_key(key.as_bytes())).await?;
    let document = node.document();
    let Some(newest) = trusted_versions(key, found.records)?.into_iter().next() else {
        // Nothing on the devices that could be read. That is "not found"
        // only while fewer than k+m devices are out, since every version
        // has a copy on k+m devices (13.3); with more out, a version may
        // be entirely out of view, and the answer is the outage. A removed
        // device is not out: nothing is expected of it (18.2.1).
        let out: Vec<&(ErrorCode, String)> = found
            .unread
            .iter()
            .filter(|(device, _)| {
                document
                    .device(**device)
                    .is_some_and(|d| d.state != DeviceState::Removed)
            })
            .map(|(_, why)| why)
            .collect();
        if out.len() >= document.k as usize + document.m as usize {
            let (code, reason) = out[0].clone();
            let out = out.len();
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                ..ErrorDetail::new(
                    code,
                    format!(
                        "no copy of key {key:?} on the devices that could be read, and {out} device(s) could not be, enough to hold every copy of a version: {reason}"
                    ),
                )
            }));
        }
        return Err(not_found(key));
    };
    let TrustedVersion {
        record,
        mut missing,
        vouching,
    } = newest;
    for copy in &mut missing {
        if copy.fault != RecordCopyFault::Missing {
            continue;
        }
        if let Some((_, reason)) = found.unread.get(&copy.device) {
            copy.fault = RecordCopyFault::Unavailable {
                reason: reason.clone(),
            };
        } else if document.device(copy.device).is_none() {
            copy.fault = RecordCopyFault::Unavailable {
                reason: format!("{} is not in the cluster document", copy.device),
            };
        }
    }
    if vouching < record.k as usize {
        // Refused with the first unavailable device's own code when there
        // is one, as 11.4 refuses shards: a node down and a copy gone call
        // for different actions.
        let code = missing
            .iter()
            .find_map(|m| match m.fault {
                RecordCopyFault::Unavailable { .. } => Some(
                    found
                        .unread
                        .get(&m.device)
                        .map(|(code, _)| *code)
                        .unwrap_or(ErrorCode::DeviceUnavailable),
                ),
                _ => None,
            })
            .unwrap_or(ErrorCode::RecordsInconsistent);
        let list: Vec<String> = missing
            .iter()
            .map(|m| format!("{}: {:?}", m.device, m.fault))
            .collect();
        return Err(Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            version: Some(record.version),
            device: missing.first().map(|m| m.device),
            ..ErrorDetail::new(
                code,
                format!(
                    "only {vouching} record copies of {} can be read and at least k = {} are needed: {}",
                    record.k as usize + record.m as usize,
                    record.k,
                    list.join("; ")
                ),
            )
        }));
    }
    Ok((record, missing))
}

// ------------------------------------------------------------ handlers

/// What one device holds (SPEC 18.2.3), from the record copies on it,
/// fetched from its node page by page as the drain's estimate fetches
/// them; no shard is read.
async fn device_contents(node: &Arc<Node>, device: DeviceId) -> Result<Response, Failure> {
    let document = node.document();
    let Some(entry) = document.device(device) else {
        return Err(Failure::Error(ErrorDetail {
            device: Some(device),
            ..ErrorDetail::new(
                ErrorCode::NotFound,
                format!("{device} is not in the cluster document"),
            )
        }));
    };
    let records = fetch_device_records(node, entry.node, device).await?;
    let mut keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut blocks = 0u64;
    let mut shard_bytes = 0u64;
    for record in &records {
        keys.insert(record.key.as_str());
        if let Ok(scheme) = record.scheme() {
            if let Some(geometry) = shard_geometry(scheme, record.block_size, record.size) {
                blocks += geometry.block_count;
            }
            shard_bytes += shard_file_length(scheme, record.block_size, record.size).unwrap_or(0);
        }
    }
    Ok(Response::DeviceContents(DeviceContents {
        device,
        node: entry.node,
        state: entry.state,
        versions: records.len() as u64,
        keys: keys.len() as u64,
        blocks,
        shard_bytes,
    }))
}

/// `Status` (19.1.3): every node's view of its devices, folded into one.
/// A node that cannot be reached does not fail it: the node is reported
/// unreachable, and its devices are listed from the document as
/// unavailable with no space, which from the cluster's side they are
/// (5.6). Status is what an operator runs to find out what is wrong, so
/// it is the one request that must work when something is.
async fn status(node: &Arc<Node>) -> Result<Response, Failure> {
    let document = node.document();
    let mut nodes = Vec::new();
    let mut devices = Vec::new();
    let mut answered = Vec::new();
    for (target, answer) in broadcast_each(node, Request::LocalStatus).await? {
        match answer {
            Ok(response) => {
                if let Response::LocalStatus { build, .. } = &response {
                    nodes.push(NodeStatus {
                        node: target,
                        reachable: true,
                        build: Some(build.clone()),
                        error: None,
                    });
                }
                answered.push((target, response));
            }
            // A node holding another document version refused the Hello:
            // that is the disagreement 6.2.7 forbids, reported as such,
            // not a node that could not be reached.
            Err(Failure::Error(detail)) if detail.code == ErrorCode::DocumentVersionMismatch => {
                return Err(Failure::Error(detail));
            }
            Err(failure) => {
                let unreachable = Unreachable::from_failure(target, failure);
                nodes.push(NodeStatus {
                    node: target,
                    reachable: false,
                    build: None,
                    error: Some(unreachable.detail.message),
                });
                // Every device the document lists for the node, removed
                // ones included (18.2.1): the status shows the whole
                // document, and what to hide is the reader's choice.
                let node_label = document.node(target).and_then(|n| n.label.clone());
                for entry in document.devices.iter().filter(|d| d.node == target) {
                    devices.push(DeviceStatus {
                        device: entry.id,
                        node: target,
                        state: entry.state,
                        available: false,
                        label: entry.label.clone(),
                        node_label: node_label.clone(),
                        total_bytes: 0,
                        free_bytes: 0,
                    });
                }
            }
        }
    }
    devices.extend(device_statuses(node, answered)?);
    // Document order, whichever node answered first.
    let position = |id: DeviceId| document.devices.iter().position(|d| d.id == id);
    devices.sort_by_key(|d| position(d.device));
    Ok(Response::Status {
        cluster_id: document.cluster_id,
        cluster_name: document.name.clone(),
        document_version: document.version,
        coordinator: node.id(),
        nodes,
        transport: document.transport,
        devices,
    })
}

/// Combine the `LocalStatus` answers of the nodes asked into one device
/// list, refusing if any node holds a different document version.
fn device_statuses(
    node: &Node,
    answers: Vec<(NodeId, Response)>,
) -> Result<Vec<DeviceStatus>, Failure> {
    let document = node.document();
    let mut devices: Vec<DeviceStatus> = Vec::new();
    for (target, response) in answers {
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
    Ok(devices)
}

async fn head_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    let (record, missing_records) = newest_readable_version(node, key).await?;
    Ok(Response::HeadObject {
        record,
        missing_records,
    })
}

async fn delete_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    check_key(node, key)?;
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

/// Remove one version from every device (14.1). Any unreachable node
/// fails the delete; a repeat succeeds because deletion is idempotent.
async fn delete_version_everywhere(
    node: &Arc<Node>,
    record: &MetadataRecord,
) -> Result<(), Failure> {
    let document = node.document();
    for shard in &record.shards {
        // A device gone from the document, or listed as removed, is never
        // written to or cleaned (18.2.1): whatever it holds is erased if
        // the disk is ever added again.
        let Some(owner) = document
            .device(shard.device)
            .filter(|d| d.state != DeviceState::Removed)
            .map(|d| d.node)
        else {
            continue;
        };
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

/// One page of the cluster's keys (15.1, 15.2.2). The client's cursor and
/// limit go down to every node, so each node returns one page of the
/// keys after the cursor, and the coordinator holds at most one page per
/// node rather than every key in the cluster.
///
/// One page from each node is enough, provided the merged page stops at
/// the last key of any node that had more. Every key such a node left
/// out is greater than the last key it returned, so nothing beyond that
/// key can be reported yet: a key from another node placed after it
/// would become the cursor and the left-out keys, being smaller, would
/// never be asked for again. Under the count limit alone the cut could
/// not pass that key anyway, but the byte bound can: a node's page may
/// stop before a long key while the merged page still has room for a
/// short key from elsewhere that sorts after it. Duplicates (one
/// version's record on k+m devices) collapse to the newest version per
/// key, which can make a page shorter than the bound; that is why the
/// truncation flag also says whether any node had more.
///
/// The listing goes around what cannot be read (15.1.1, 5.6, 13.1): a
/// node that cannot be reached, or a device its node cannot read,
/// contributes no keys and is named in the page instead, and the page
/// says whether every key can still appear: it can while fewer than k+m
/// devices are unread, since every version has a record copy on k+m
/// devices, and may not once that many are out.
async fn list_keys(node: &Arc<Node>, query: ListQuery) -> Result<Response, Failure> {
    let document = node.document();
    let mut newest: BTreeMap<String, KeyEntry> = BTreeMap::new();
    let mut any_node_truncated = false;
    let mut unread: Vec<UnavailableDevice> = Vec::new();
    // The smallest "last key" among the nodes that had more: the merged
    // page may not go beyond it.
    let mut horizon: Option<String> = None;
    for (target, answer) in broadcast_each(node, Request::LocalList(query.clone())).await? {
        match answer {
            Err(Failure::Error(detail)) if detail.code == ErrorCode::NodeUnreachable => {
                unread.extend(
                    document
                        .devices
                        .iter()
                        .filter(|d| d.node == target && d.state != DeviceState::Removed)
                        .map(|d| UnavailableDevice {
                            device: d.id,
                            node: target,
                        }),
                );
            }
            Err(f) => return Err(f),
            Ok(Response::LocalList {
                entries,
                truncated,
                unread: unread_here,
            }) => {
                unread.extend(unread_here.into_iter().map(|device| UnavailableDevice {
                    device,
                    node: target,
                }));
                any_node_truncated |= truncated;
                if truncated {
                    if let Some(last) = entries.last() {
                        let closer = horizon.as_ref().is_none_or(|h| last.key < *h);
                        if closer {
                            horizon = Some(last.key.clone());
                        }
                    }
                }
                for item in entries {
                    match newest.get(&item.key) {
                        Some(existing) if existing.version >= item.version => {}
                        _ => {
                            newest.insert(item.key.clone(), item);
                        }
                    }
                }
            }
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{target} answered LocalList with {other:?}"),
                ))
            }
        }
    }
    let mut keys: Vec<KeyEntry> = newest.into_values().collect();
    if let Some(horizon) = &horizon {
        keys.retain(|e| e.key <= *horizon);
    }
    let (keys, cut) = crate::local_ops::page_of_keys(keys, query.limit);
    unread.sort_by_key(|u| (u.node, u.device));
    let complete = unread.len() < document.k as usize + document.m as usize;
    Ok(Response::ListKeys {
        keys,
        truncated: cut || any_node_truncated,
        unread,
        complete,
    })
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

/// Open a block stream for each of `indices` from `first_block` on, so
/// that a device that cannot serve fails the request before any block is
/// read (11.4).
async fn open_shard_sources(
    node: &Arc<Node>,
    document: &ClusterDocument,
    record: &MetadataRecord,
    key: &str,
    indices: &[ShardIndex],
    first_block: u64,
    block_count: u64,
) -> Result<Vec<ShardSource>, Failure> {
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let mut sources: Vec<ShardSource> = Vec::with_capacity(indices.len());
    for &index in indices {
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
                first_block,
                block_count,
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
        match connection.read_response(request_id).await {
            Ok(Response::GetShard { .. }) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{owner} answered GetShard with {other:?}"),
                ))
            }
            Err(e) => return Err(remote_failure(owner, e)),
        }
        sources.push(ShardSource {
            index,
            device,
            owner,
            connection,
            request_id,
        });
    }
    debug_assert!(indices.len() <= scheme.total_shards());
    Ok(sources)
}

/// One block for `stripe` from each source, in source order. A stream
/// that ends early, or out of order, is the error it names.
async fn read_stripe_blocks(
    sources: &mut [ShardSource],
    stripe: u64,
    key: &str,
    record: &MetadataRecord,
) -> Result<Vec<ShardBlock>, ErrorDetail> {
    let mut received: Vec<ShardBlock> = Vec::with_capacity(sources.len());
    for source in sources.iter_mut() {
        let context = |detail: ErrorDetail| ErrorDetail {
            node: Some(source.owner),
            device: Some(source.device),
            key: Some(key.to_string()),
            version: Some(record.version),
            shard_index: Some(source.index.0),
            stripe: Some(stripe),
            ..detail
        };
        match source.connection.read_stream_item(source.request_id).await {
            Ok(StreamItem::Data(data)) => {
                if data.sequence != stripe {
                    return Err(context(ErrorDetail::new(
                        ErrorCode::ProtocolViolation,
                        format!(
                            "node sent block {} when {stripe} was expected",
                            data.sequence
                        ),
                    )));
                }
                received.push(ShardBlock {
                    index: source.index,
                    bytes: data.bytes,
                    checksum: data.checksum,
                });
            }
            Ok(StreamItem::End(end)) => {
                return Err(context(end.error.unwrap_or_else(|| {
                    ErrorDetail::new(
                        ErrorCode::ProtocolViolation,
                        "node ended its stream early".to_string(),
                    )
                })));
            }
            Err(e) => {
                let Failure::Error(detail) = remote_failure(source.owner, e) else {
                    unreachable!("remote_failure always yields an error detail")
                };
                return Err(detail);
            }
        }
    }
    Ok(received)
}

/// A shard a read could not open and will reconstruct around from the
/// first stripe (11.4), and why. `Unavailable` may be temporary; the
/// others will not mend themselves.
struct UnreadableShard {
    index: ShardIndex,
    device: DeviceId,
    fault: FaultKind,
    /// The stripe from which the shard was reconstructed around: 0 for a
    /// data shard, or the damaged stripe at which parity was first wanted.
    first_stripe: u64,
}

/// Open the shards of `indices` from `first_block` on. One that cannot be
/// opened is returned separately, to be reconstructed around, rather than
/// failing the read (11.4): a missing or unreadable file, a device that
/// is unavailable, or a node that cannot be reached. A node that answers
/// wrongly is still an error.
async fn open_shards(
    node: &Arc<Node>,
    document: &ClusterDocument,
    record: &MetadataRecord,
    key: &str,
    indices: &[ShardIndex],
    first_block: u64,
    block_count: u64,
) -> Result<(Vec<ShardSource>, Vec<UnreadableShard>), Failure> {
    let mut sources = Vec::with_capacity(indices.len());
    let mut unreadable = Vec::new();
    for &index in indices {
        let device = record
            .device_for(index)
            .expect("validated record lists every index");
        match open_shard_sources(
            node,
            document,
            record,
            key,
            &[index],
            first_block,
            block_count,
        )
        .await
        {
            Ok(opened) => sources.extend(opened),
            Err(Failure::Error(detail)) => {
                let fault = match detail.code {
                    ErrorCode::NotFound => FaultKind::Missing,
                    ErrorCode::DeviceUnavailable | ErrorCode::NodeUnreachable => {
                        FaultKind::Unavailable {
                            reason: detail.message,
                        }
                    }
                    ErrorCode::ProtocolViolation | ErrorCode::Internal => {
                        return Err(Failure::Error(detail))
                    }
                    _ => FaultKind::Unreadable {
                        reason: detail.message,
                    },
                };
                unreadable.push(UnreadableShard {
                    index,
                    device,
                    fault,
                    first_stripe: first_block,
                });
            }
            Err(other) => return Err(other),
        }
    }
    Ok((sources, unreadable))
}

/// The refusal when more than m shards cannot be read (11.4, 16.1): the
/// first shard's own code, since a node down and a file gone call for
/// different actions, and the whole list in the message.
fn too_many_unreadable(
    key: &str,
    record: &MetadataRecord,
    unreadable: &[UnreadableShard],
) -> ErrorDetail {
    let first = &unreadable[0];
    let code = match &first.fault {
        FaultKind::Missing => ErrorCode::NotFound,
        FaultKind::Unavailable { .. } => ErrorCode::DeviceUnavailable,
        _ => ErrorCode::BlockChecksumMismatch,
    };
    let list: Vec<String> = unreadable
        .iter()
        .map(|e| format!("shard {} on {}: {:?}", e.index.0, e.device, e.fault))
        .collect();
    ErrorDetail {
        device: Some(first.device),
        key: Some(key.to_string()),
        version: Some(record.version),
        shard_index: Some(first.index.0),
        ..ErrorDetail::new(
            code,
            format!(
                "{} of {} shards cannot be read and at most {} may be: {}",
                unreadable.len(),
                record.k as usize + record.m as usize,
                record.m,
                list.join("; ")
            ),
        )
    }
}

async fn get_object(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    key: &str,
) -> Result<(), Failure> {
    let (record, missing_records) = match newest_readable_version(node, key).await {
        Ok(found) => found,
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
    // An empty object has no stripes and reads no shard; its geometry is
    // never used, so any valid one will do.
    let geometry = match shard_geometry(scheme, record.block_size, record.size.max(1)) {
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

    // Open one block stream per data shard before answering the client.
    // A shard that cannot be opened is reconstructed around from the
    // first stripe and parity is opened at once; more than m such shards
    // is the refusal (11.4).
    // Parity is otherwise not read unless a block turns out damaged.
    let data_indices = scheme.data_shard_indices();
    let all_indices = scheme.shard_indices();
    let parity_indices: Vec<ShardIndex> = all_indices
        .iter()
        .copied()
        .filter(|i| !data_indices.contains(i))
        .collect();
    let mut sources: Vec<ShardSource> = Vec::new();
    let mut unreadable: Vec<UnreadableShard> = Vec::new();
    let mut parity_open = false;
    if record.size > 0 {
        let opened = open_shards(
            node,
            &document,
            &record,
            key,
            &data_indices,
            0,
            geometry.block_count,
        )
        .await;
        match opened {
            Ok((opened, missing)) => {
                sources.extend(opened);
                unreadable.extend(missing);
            }
            Err(f) => return respond(writer, id, Err(f)).await,
        }
        if !unreadable.is_empty() {
            let opened = open_shards(
                node,
                &document,
                &record,
                key,
                &parity_indices,
                0,
                geometry.block_count,
            )
            .await;
            match opened {
                Ok((opened, missing)) => {
                    sources.extend(opened);
                    unreadable.extend(missing);
                }
                Err(f) => return respond(writer, id, Err(f)).await,
            }
            parity_open = true;
        }
        if unreadable.len() > scheme.parity_shards() {
            let detail = too_many_unreadable(key, &record, &unreadable);
            return respond(writer, id, Err(Failure::Error(detail))).await;
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
    let stripe_size = scheme.data_shards() as u64 * record.block_size;
    let stripe_count = record.size.div_ceil(stripe_size.max(1));
    let mut reconstructed: Vec<Reconstruction> = Vec::new();
    let mut hasher = Xxh3::new();
    let mut delivered: u64 = 0;
    let mut sequence: u64 = 0;
    for stripe in 0..stripe_count {
        let mut received = match read_stripe_blocks(&mut sources, stripe, key, &record).await {
            Ok(blocks) => blocks,
            Err(detail) => return Err(fail_and_close(writer, id, detail).await),
        };
        let stripe_len = (record.size - stripe * stripe_size).min(stripe_size) as usize;
        // Only the shards with a stream are asked for; one that could not
        // be opened is simply not among them, and the decoder rebuilds the
        // data from whatever k it has.
        let requested: Vec<ShardIndex> = sources.iter().map(|s| s.index).collect();
        let internal = |e: djbod_core::stripe::StripeError| ErrorDetail {
            key: Some(key.to_string()),
            version: Some(record.version),
            stripe: Some(stripe),
            ..ErrorDetail::new(ErrorCode::Internal, e.to_string())
        };
        let mut decoded = match decode_stripe(&code, &requested, &received, stripe_len) {
            Ok(decoded) => decoded,
            Err(e) => return Err(fail_and_close(writer, id, internal(e)).await),
        };
        if !parity_open && !matches!(decoded, DecodedStripe::Intact { .. }) {
            // A damaged block: fetch the parity shards from this stripe
            // on and decode again from every block (11.4). Nothing is
            // written to any device; the damage is reported at the end.
            let opened = open_shards(
                node,
                &document,
                &record,
                key,
                &parity_indices,
                stripe,
                geometry.block_count - stripe,
            )
            .await;
            let mut parity = match opened {
                Ok((opened, missing)) => {
                    unreadable.extend(missing);
                    opened
                }
                Err(Failure::Error(detail)) => return Err(fail_and_close(writer, id, detail).await),
                Err(other) => return Err(other),
            };
            parity_open = true;
            match read_stripe_blocks(&mut parity, stripe, key, &record).await {
                Ok(blocks) => received.extend(blocks),
                Err(detail) => return Err(fail_and_close(writer, id, detail).await),
            }
            sources.extend(parity);
            let requested: Vec<ShardIndex> = sources.iter().map(|s| s.index).collect();
            decoded = match decode_stripe(&code, &requested, &received, stripe_len) {
                Ok(decoded) => decoded,
                Err(e) => return Err(fail_and_close(writer, id, internal(e)).await),
            };
        }
        let data = match decoded {
            DecodedStripe::Intact { data } => data,
            DecodedStripe::Repaired { data, faults } => {
                for fault in faults {
                    if unreadable.iter().any(|u| u.index == fault.index) {
                        continue; // reported once for every stripe below
                    }
                    reconstructed.push(Reconstruction {
                        shard_index: fault.index.0,
                        device: record
                            .device_for(fault.index)
                            .expect("validated record lists every index"),
                        fault: fault.kind,
                        first_stripe: stripe,
                        stripes: 1,
                    });
                }
                data
            }
            DecodedStripe::Unrecoverable {
                usable,
                needed,
                faults,
            } => {
                let first = &faults[0];
                let detail = ErrorDetail {
                    device: record.device_for(first.index),
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(first.index.0),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(
                        ErrorCode::BlockChecksumMismatch,
                        format!(
                            "{} damaged block(s) in stripe {stripe}, {usable} usable of {needed} needed; first: {:?}",
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
    // A shard that could not be opened was reconstructed around in every
    // stripe from the one where it was first wanted.
    for shard in unreadable {
        reconstructed.push(Reconstruction {
            shard_index: shard.index.0,
            device: shard.device,
            fault: shard.fault,
            first_stripe: shard.first_stripe,
            stripes: stripe_count - shard.first_stripe,
        });
    }

    // Every node ends its stream; a node reporting an error here means
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
                        format!("node did not end its stream cleanly: {other:?}"),
                    )
                };
                return Err(fail_and_close(writer, id, detail).await);
            }
        }
    }

    // The whole-object check (11.7), delivered as the stream's status,
    // with what was reconstructed on the way (11.4) and the record
    // copies the lookup went without (9.4.4).
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
            end: StreamEnd {
                error: None,
                object_size: None,
                object_checksum: None,
                reconstructed,
                missing_records,
            },
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

struct ShardWriter {
    index: ShardIndex,
    device: DeviceId,
    owner: NodeId,
    connection: Connection,
    request_id: u32,
}

/// Choose k+m distinct active devices with room for a shard file, most
/// free space first, ties by device id (10.4, 10.5). A device its node
/// cannot read (5.6) is left out like a full one; the write goes ahead
/// on the rest if k+m remain, and is refused, naming what is missing,
/// if not. The snapshot may be stale by the time the shards are written;
/// that is the write's failure to report (10.7), not a reason to check.
fn place(
    statuses: &[DeviceStatus],
    scheme: Scheme,
    shard_bytes: u64,
) -> Result<Vec<DeviceStatus>, Failure> {
    let mut eligible: Vec<&DeviceStatus> = statuses
        .iter()
        .filter(|d| d.state == DeviceState::Active && d.available && d.free_bytes >= shard_bytes)
        .collect();
    eligible.sort_by(|a, b| {
        b.free_bytes
            .cmp(&a.free_bytes)
            .then(a.device.cmp(&b.device))
    });
    if eligible.len() < scheme.total_shards() {
        let unavailable: Vec<String> = statuses
            .iter()
            .filter(|d| d.state == DeviceState::Active && !d.available)
            .map(|d| d.device.0.to_string())
            .collect();
        let missing = if unavailable.is_empty() {
            String::new()
        } else {
            format!(
                "; {} active device(s) unavailable: {}",
                unavailable.len(),
                unavailable.join(", ")
            )
        };
        return Err(error(
            ErrorCode::InsufficientDevices,
            format!(
                "{} active devices have {shard_bytes} bytes free; {} are needed{missing}",
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

async fn abort_shard_writers(writers: &mut [ShardWriter], key_hash: KeyHash, version: VersionId) {
    for writer in writers.iter_mut() {
        // Best effort. Dropping the connection also drops any temporary on
        // the node's side.
        let _ = writer
            .connection
            .request(Request::AbortShard {
                device: writer.device,
                key_hash,
                version,
                shard_index: writer.index.0,
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
    let PreparedPut {
        scheme,
        record: record_template,
        mut writers,
        unavailable,
    } = match prepared {
        Ok(p) => p,
        Err(Failure::Error(detail)) => return Err(fail_and_close(writer, id, detail).await),
        Err(other) => return Err(other),
    };
    let key_hash = record_template.key_hash;
    let version = record_template.version;

    let outcome = stream_body_to_writers(
        reader,
        &mut writers,
        scheme,
        &record_template,
        node.stream_idle_timeout(),
    )
    .await;
    let object_checksum = match outcome {
        Ok(checksum) => checksum,
        Err(Failure::Error(detail)) => {
            abort_shard_writers(&mut writers, key_hash, version).await;
            return Err(fail_and_close(writer, id, detail).await);
        }
        Err(other) => {
            abort_shard_writers(&mut writers, key_hash, version).await;
            return Err(other);
        }
    };

    let record = MetadataRecord {
        object_checksum,
        ..record_template
    };
    if let Err(f) = write_records(&mut writers, &record).await {
        abort_shard_writers(&mut writers, key_hash, version).await;
        return match f {
            Failure::Error(detail) => Err(fail_and_close(writer, id, detail).await),
            other => Err(other),
        };
    }
    drop(writers);

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
    respond(
        writer,
        id,
        Ok(Response::PutObject {
            version,
            unavailable,
        }),
    )
    .await
}

/// A write ready for its body: the scheme, the record to complete, a
/// writer per chosen device, and the devices placement went around (5.6).
struct PreparedPut {
    scheme: Scheme,
    record: MetadataRecord,
    writers: Vec<ShardWriter>,
    unavailable: Vec<UnavailableDevice>,
}

/// Everything before the first body byte is consumed: checks, placement,
/// and a writer to each device ready to receive.
async fn prepare_put(
    node: &Arc<Node>,
    versions: &VersionGenerator,
    params: &PutParams,
) -> Result<PreparedPut, Failure> {
    check_key(node, &params.key)?;
    if let Some(content_type) = &params.content_type {
        if content_type.len() > MAX_CONTENT_TYPE_BYTES {
            return Err(error(
                ErrorCode::MetadataTooLarge,
                format!(
                    "content type is {} bytes; the limit is {MAX_CONTENT_TYPE_BYTES}",
                    content_type.len()
                ),
            ));
        }
    }
    let document = node.document();
    let metadata_bytes: u64 = params
        .user_metadata
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum();
    if metadata_bytes > document.max_user_metadata_bytes {
        return Err(error(
            ErrorCode::MetadataTooLarge,
            format!(
                "user metadata is {metadata_bytes} bytes of keys and values; the cluster's limit is {} (max_user_metadata_bytes)",
                document.max_user_metadata_bytes
            ),
        ));
    }
    if params.size > document.max_object_bytes {
        return Err(error(
            ErrorCode::ObjectTooLarge,
            format!(
                "object of {} bytes exceeds the cluster's maximum of {} (max_object_bytes)",
                params.size, document.max_object_bytes
            ),
        ));
    }
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
    let unavailable: Vec<UnavailableDevice> = statuses
        .iter()
        .filter(|d| d.state == DeviceState::Active && !d.available)
        .map(|d| UnavailableDevice {
            device: d.device,
            node: d.node,
        })
        .collect();
    let version = versions.next();

    let mut writers = Vec::with_capacity(chosen.len());
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
        writers.push(ShardWriter {
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
        revision: 0,
    };
    Ok(PreparedPut {
        scheme,
        record,
        writers,
        unavailable,
    })
}

/// Read the client's body stream, encode it stripe by stripe, fan the
/// blocks out to the devices, and finish every shard. Returns the
/// whole-object checksum.
async fn stream_body_to_writers(
    reader: &mut Reader,
    writers: &mut [ShardWriter],
    scheme: Scheme,
    record: &MetadataRecord,
    idle: std::time::Duration,
) -> Result<BlockChecksum, Failure> {
    let code = ReedSolomonCode::new(scheme);
    let stripe_size = scheme.data_shards() * record.block_size as usize;
    let mut hasher = Xxh3::new();
    let mut received: u64 = 0;
    let mut expected_sequence: u64 = 0;
    let mut stripe_buffer: Vec<u8> = Vec::with_capacity(stripe_size.min(1 << 26));
    let mut stripe_number: u64 = 0;

    loop {
        // A client that stops sending must not hold k+m shard writes open
        // forever; the caller aborts the shard writes on this error (10.12).
        let message = read_message_within(reader, idle).await?;
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
                        send_stripe(writers, &code, &stripe_buffer, record, stripe_number).await?;
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
                    send_stripe(writers, &code, &stripe_buffer, record, stripe_number).await?;
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
        for writer in writers.iter_mut() {
            writer
                .connection
                .send_end(
                    writer.request_id,
                    StreamEnd {
                        error: None,
                        object_size: Some(record.size),
                        object_checksum: Some(object_checksum),
                        reconstructed: Vec::new(),
                        missing_records: Vec::new(),
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
    }
    Ok(object_checksum)
}

async fn send_stripe(
    writers: &mut [ShardWriter],
    code: &ReedSolomonCode,
    stripe: &[u8],
    record: &MetadataRecord,
    stripe_number: u64,
) -> Result<(), Failure> {
    let blocks = encode_stripe(code, stripe, record.block_size as usize)
        .map_err(|e| error(ErrorCode::Internal, format!("encode failed: {e}")))?;
    for (writer, block) in writers.iter_mut().zip(blocks) {
        debug_assert_eq!(writer.index, block.index);
        writer
            .connection
            .send_data(
                writer.request_id,
                DataFrame {
                    sequence: stripe_number,
                    checksum: block.checksum,
                    bytes: block.bytes,
                },
            )
            .await
            .map_err(|e| remote_failure(writer.owner, e))?;
    }
    Ok(())
}

async fn write_records(
    writers: &mut [ShardWriter],
    record: &MetadataRecord,
) -> Result<(), Failure> {
    for writer in writers.iter_mut() {
        match writer
            .connection
            .request(Request::PutMeta {
                device: writer.device,
                record: record.clone(),
            })
            .await
        {
            Ok(Response::PutMeta) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{} answered PutMeta with {other:?}", writer.owner),
                ))
            }
            Err(e) => return Err(remote_failure(writer.owner, e)),
        }
    }
    Ok(())
}

// --------------------------------------------------------------- REPAIR

/// One shard as the repair sees it: a stream of blocks from its device,
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
/// the record's checksum. Fail-stop: an unreachable node or more than m
/// damaged shards is an error and nothing is changed.
/// The newest version of `key` as repair needs it: the record copies
/// that exist must agree and there must be at least k of them, but they
/// need not be all k+m. Returns the record and the devices whose copy is
/// missing (SPEC 18.4.2). A read trusts the same set
/// (`newest_readable_version`) and reports what is missing; repair is
/// the operation that completes it.
async fn repairable_record(
    node: &Arc<Node>,
    key: &str,
) -> Result<(MetadataRecord, Vec<DeviceId>, Vec<(DeviceId, u64)>), Failure> {
    check_key(node, key)?;
    let located = lookup(node, hash_key(key.as_bytes())).await?;
    let mut by_version: BTreeMap<VersionId, Vec<LocatedRecord>> = BTreeMap::new();
    for item in located {
        by_version
            .entry(item.record.version)
            .or_default()
            .push(item);
    }
    let Some((version, copies)) = by_version.into_iter().next_back() else {
        return Err(Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            ..ErrorDetail::new(ErrorCode::NotFound, format!("no object under key {key:?}"))
        }));
    };
    for copy in &copies {
        if copy.record.key != key {
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(version),
                ..ErrorDetail::new(
                    ErrorCode::KeyMismatch,
                    format!(
                        "record under this key hash names key {:?}, not {key:?}",
                        copy.record.key
                    ),
                )
            }));
        }
    }
    // The highest revision is the truth to complete towards (18.8.1).
    let current_revision = copies
        .iter()
        .map(|c| c.record.revision)
        .max()
        .expect("non-empty");
    let current: Vec<&LocatedRecord> = copies
        .iter()
        .filter(|c| c.record.revision == current_revision)
        .collect();
    let first = current[0].record.clone();
    for copy in &current {
        if copy.record != first {
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(version),
                device: Some(copy.device),
                ..ErrorDetail::new(
                    ErrorCode::RecordsInconsistent,
                    format!("record copies at revision {current_revision} disagree; repair cannot choose between them"),
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
    // Lower-revision copies must describe the same body; otherwise
    // something other than a re-placement produced them.
    for copy in &copies {
        if copy.record.revision < current_revision && !copy.record.same_body(&first) {
            return Err(Failure::Error(ErrorDetail {
                key: Some(key.to_string()),
                version: Some(version),
                device: Some(copy.device),
                ..ErrorDetail::new(
                    ErrorCode::RecordsInconsistent,
                    format!(
                        "a revision {} copy describes a different body from revision {current_revision}",
                        copy.record.revision
                    ),
                )
            }));
        }
    }
    // Trust needs at least k agreeing copies. During an interrupted
    // re-placement the new revision may have only one; the older revision's
    // copies describe the same body and vouch for it (18.8.1).
    let vouching = copies
        .iter()
        .filter(|c| c.record.revision == current_revision || first.same_body(&c.record))
        .count();
    if vouching < first.k as usize {
        return Err(Failure::Error(ErrorDetail {
            key: Some(key.to_string()),
            version: Some(version),
            ..ErrorDetail::new(
                ErrorCode::RecordsInconsistent,
                format!(
                    "only {vouching} record copies remain of {}; fewer than k = {} cannot be trusted",
                    first.shards.len(),
                    first.k
                ),
            )
        }));
    }
    let missing: Vec<DeviceId> = first
        .shards
        .iter()
        .map(|s| s.device)
        .filter(|d| !current.iter().any(|c| c.device == *d))
        .collect();
    let stale = stale_copies(&first, &copies);
    Ok((first, missing, stale))
}

/// Write the record to every device in `missing` (18.4.2).
async fn rewrite_record_copies(
    node: &Arc<Node>,
    record: &MetadataRecord,
    missing: &[DeviceId],
) -> Result<(), Failure> {
    let document = node.document();
    for device in missing {
        let owner = document.device(*device).map(|d| d.node).ok_or_else(|| {
            Failure::Error(ErrorDetail {
                device: Some(*device),
                key: Some(record.key.clone()),
                version: Some(record.version),
                ..ErrorDetail::new(
                    ErrorCode::DeviceUnavailable,
                    format!("{device} is not in the cluster document"),
                )
            })
        })?;
        let mut connection = connect_to(node, owner).await?;
        match connection
            .request(Request::PutMeta {
                device: *device,
                record: record.clone(),
            })
            .await
        {
            Ok(Response::PutMeta) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{owner} answered PutMeta with {other:?}"),
                ))
            }
            Err(e) => return Err(remote_failure(owner, e)),
        }
    }
    Ok(())
}

/// Remove stale lower-revision copies (record and shard) from devices
/// the current record no longer lists (18.8.1).
async fn remove_stale_copies(
    node: &Arc<Node>,
    record: &MetadataRecord,
    stale: &[(DeviceId, u64)],
) -> Result<Vec<DeviceId>, Failure> {
    let document = node.document();
    let mut removed = Vec::new();
    for (device, _) in stale {
        let Some(owner) = document
            .device(*device)
            .filter(|d| d.state != DeviceState::Removed)
            .map(|d| d.node)
        else {
            continue; // device gone from the cluster, or removed; nothing to clean
        };
        let mut connection = connect_to(node, owner).await?;
        connection
            .request(Request::DeleteVersion {
                device: *device,
                key_hash: record.key_hash,
                version: record.version,
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
        removed.push(*device);
    }
    Ok(removed)
}

async fn repair_object(node: &Arc<Node>, key: &str) -> Result<Response, Failure> {
    let (record, missing_record_copies, stale) = repairable_record(node, key).await?;
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let document = node.document();
    // Devices that have left the document (6.2.6.3) or are listed as
    // removed (18.2.1.1) hold nothing the cluster will use again: their
    // record copies are not rewritten and their shards are rebuilt onto
    // other devices (18.3).
    let in_service = |device: DeviceId| {
        document
            .device(device)
            .is_some_and(|d| d.state != DeviceState::Removed)
    };
    let lost: Vec<ShardIndex> = record
        .shards
        .iter()
        .filter(|s| !in_service(s.device))
        .map(|s| ShardIndex(s.index))
        .collect();
    let missing_record_copies: Vec<DeviceId> = missing_record_copies
        .into_iter()
        .filter(|d| in_service(*d))
        .collect();

    if record.size == 0 {
        // No shard files exist; only the record's placement needs mending.
        let (next, relocations) = relocate_lost_shards(node, &record, &lost).await?;
        let rewritten_copies = if relocations.is_empty() {
            rewrite_record_copies(node, &record, &missing_record_copies).await?;
            missing_record_copies
        } else {
            let devices = devices_new_first(&next, &relocations);
            rewrite_record_copies(node, &next, &devices).await?;
            devices
        };
        let stale_copies_removed = remove_stale_copies(node, &record, &stale).await?;
        let shards = record
            .shards
            .iter()
            .map(|shard| {
                let relocated_to = relocations
                    .iter()
                    .find(|(i, _)| i.0 == shard.index)
                    .map(|(_, d)| *d);
                ShardRepair {
                    index: shard.index,
                    device: shard.device,
                    condition: if relocated_to.is_some() {
                        ShardCondition::Lost
                    } else {
                        ShardCondition::Intact
                    },
                    rewritten: false,
                    relocated_to,
                }
            })
            .collect();
        return Ok(Response::RepairObject(RepairReport {
            key: key.to_string(),
            version: record.version,
            shards,
            record_copies_rewritten: rewritten_copies,
            stale_copies_removed,
        }));
    }
    let geometry = shard_geometry(scheme, record.block_size, record.size).ok_or_else(|| {
        error(
            ErrorCode::RecordsInconsistent,
            "record has an impossible size",
        )
    })?;

    let mut sources = open_repair_sources(node, &record, geometry.block_count).await?;

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
        rewrite_record_copies(node, &record, &missing_record_copies).await?;
        let stale_copies_removed = remove_stale_copies(node, &record, &stale).await?;
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
                    relocated_to: None,
                })
                .collect(),
            record_copies_rewritten: missing_record_copies,
            stale_copies_removed,
        }));
    }

    // Pass 2: rebuild every damaged shard from the intact ones: onto its
    // own device, or onto a freshly chosen one when its device has left
    // the document. The old files are replaced only when each PutShard
    // finishes.
    let (next, relocations) = relocate_lost_shards(node, &record, &lost).await?;
    let targets: Vec<(ShardIndex, DeviceId)> = damaged
        .iter()
        .map(|&i| {
            let index = sources[i].index;
            let destination = relocations
                .iter()
                .find(|(j, _)| *j == index)
                .map(|(_, d)| *d)
                .unwrap_or(sources[i].device);
            (index, destination)
        })
        .collect();
    rebuild_shards(node, key, &record, &sources, &targets).await?;

    // Shards first, record copies second: a crash between the two leaves a
    // shard without a record, which the scrub reports and a later repair
    // completes. A relocation moves the record on by one revision, written
    // to the new devices first like a re-placement (18.8.2).
    let rewritten_copies = if relocations.is_empty() {
        rewrite_record_copies(node, &record, &missing_record_copies).await?;
        missing_record_copies
    } else {
        let devices = devices_new_first(&next, &relocations);
        rewrite_record_copies(node, &next, &devices).await?;
        devices
    };
    let stale_copies_removed = remove_stale_copies(node, &record, &stale).await?;

    let rewritten: Vec<ShardIndex> = targets.iter().map(|(i, _)| *i).collect();
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
                relocated_to: relocations
                    .iter()
                    .find(|(i, _)| *i == s.index)
                    .map(|(_, d)| *d),
            })
            .collect(),
        record_copies_rewritten: rewritten_copies,
        stale_copies_removed,
    }))
}

/// Choose a new device for each lost shard as a write would (10.4, 10.5):
/// active, with room for the shard file, holding no shard of the version,
/// most free space first, all distinct. Returns the record at the next
/// revision with the new placement, and the (index, device) pairs chosen;
/// with nothing lost the record is returned unchanged and the list empty.
async fn relocate_lost_shards(
    node: &Arc<Node>,
    record: &MetadataRecord,
    lost: &[ShardIndex],
) -> Result<(MetadataRecord, Vec<(ShardIndex, DeviceId)>), Failure> {
    if lost.is_empty() {
        return Ok((record.clone(), Vec::new()));
    }
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let shard_bytes = if record.size == 0 {
        0
    } else {
        shard_file_length(scheme, record.block_size, record.size)
            .ok_or_else(|| error(ErrorCode::Internal, "cannot size shard file"))?
    };
    let devices: Vec<DeviceId> = record.shards.iter().map(|s| s.device).collect();
    let statuses = device_statuses(node, broadcast(node, Request::LocalStatus).await?)?;
    let mut ranked: Vec<DeviceStatus> = statuses
        .into_iter()
        .filter(|d| {
            d.state == DeviceState::Active
                && d.available
                && d.free_bytes >= shard_bytes
                && !devices.contains(&d.device)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.free_bytes
            .cmp(&a.free_bytes)
            .then(a.device.cmp(&b.device))
    });
    if ranked.len() < lost.len() {
        return Err(Failure::Error(ErrorDetail {
            key: Some(record.key.clone()),
            version: Some(record.version),
            ..ErrorDetail::new(
                ErrorCode::InsufficientDevices,
                format!(
                    "{} shard(s) are on devices no longer in the cluster, but only {} active device(s) with {shard_bytes} bytes free hold no shard of this version",
                    lost.len(),
                    ranked.len()
                ),
            )
        }));
    }
    let mut next = record.clone();
    next.revision += 1;
    let mut relocations = Vec::with_capacity(lost.len());
    for (index, status) in lost.iter().zip(ranked) {
        for shard in next.shards.iter_mut() {
            if shard.index == index.0 {
                shard.device = status.device;
            }
        }
        relocations.push((*index, status.device));
    }
    Ok((next, relocations))
}

/// Every device of `record`, the newly chosen ones first.
fn devices_new_first(
    record: &MetadataRecord,
    relocations: &[(ShardIndex, DeviceId)],
) -> Vec<DeviceId> {
    let new_devices: Vec<DeviceId> = relocations.iter().map(|(_, d)| *d).collect();
    let mut order = new_devices.clone();
    order.extend(
        record
            .shards
            .iter()
            .map(|s| s.device)
            .filter(|d| !new_devices.contains(d)),
    );
    order
}

/// Open a block stream from every device of `record`. A device that
/// cannot open the file is a damaged shard, not a failure; an unreachable
/// device is a failure.
async fn open_repair_sources(
    node: &Arc<Node>,
    record: &MetadataRecord,
    block_count: u64,
) -> Result<Vec<RepairSource>, Failure> {
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let document = node.document();
    let mut sources: Vec<RepairSource> = Vec::with_capacity(scheme.total_shards());
    for index in scheme.shard_indices() {
        let device = record
            .device_for(index)
            .expect("validated record lists every index");
        // A device that has left the document (6.2.6.3), or is listed
        // as removed (18.2.1), is a lost shard, to be rebuilt elsewhere
        // (18.3), not a failure.
        let Some(owner) = document
            .device(device)
            .filter(|d| d.state != DeviceState::Removed)
            .map(|d| d.node)
        else {
            sources.push(RepairSource {
                index,
                device,
                owner: node.id(),
                stream: None,
                condition: ShardCondition::Lost,
            });
            continue;
        };
        let mut connection = connect_to(node, owner).await?;
        let request_id = connection
            .send_request(Request::GetShard {
                device,
                key_hash: record.key_hash,
                version: record.version,
                shard_index: index.0,
                first_block: 0,
                block_count,
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
            Err(ConnectionError::Remote(detail)) => (
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
    Ok(sources)
}

/// Write the shards named in `targets` (index, destination device) by
/// streaming the intact sources again, decoding each stripe, and
/// re-encoding. Used by repair (destination = the shard's own device) and
/// by re-placement (destination = a new device). Every destination file is
/// complete or absent.
async fn rebuild_shards(
    node: &Arc<Node>,
    key: &str,
    record: &MetadataRecord,
    sources: &[RepairSource],
    targets: &[(ShardIndex, DeviceId)],
) -> Result<(), Failure> {
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let geometry = shard_geometry(scheme, record.block_size, record.size).ok_or_else(|| {
        error(
            ErrorCode::RecordsInconsistent,
            "record has an impossible size",
        )
    })?;
    let document = node.document();
    let code = ReedSolomonCode::new(scheme);
    let all_indices = scheme.shard_indices();
    let stripe_size = scheme.data_shards() as u64 * record.block_size;

    let mut writers: Vec<ShardWriter> = Vec::with_capacity(targets.len());
    for (index, device) in targets {
        let owner = document.device(*device).map(|d| d.node).ok_or_else(|| {
            Failure::Error(ErrorDetail {
                device: Some(*device),
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
            .send_request(Request::PutShard {
                device: *device,
                key_hash: record.key_hash,
                version: record.version,
                shard_index: index.0,
                k: scheme.data_shards() as u8,
                m: scheme.parity_shards() as u8,
                block_length: record.block_size,
                object_size: record.size,
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
        match connection.read_response(request_id).await {
            Ok(Response::PutShardReady) => {}
            Ok(other) => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{owner} answered PutShard with {other:?}"),
                ))
            }
            Err(e) => return Err(remote_failure(owner, e)),
        }
        writers.push(ShardWriter {
            index: *index,
            device: *device,
            owner,
            connection,
            request_id,
        });
    }
    let mut readers = reopen_intact_streams(node, sources, record, geometry.block_count).await?;
    for stripe in 0..geometry.block_count {
        let received = read_repair_stripe(&mut readers, stripe, key, record).await?;
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
    finish_repair_streams(&mut readers, key, record).await?;
    for writer in writers.iter_mut() {
        writer
            .connection
            .send_end(
                writer.request_id,
                StreamEnd {
                    error: None,
                    object_size: Some(record.size),
                    object_checksum: Some(record.object_checksum),
                    reconstructed: Vec::new(),
                    missing_records: Vec::new(),
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
    Ok(())
}

// ------------------------------------------------------------ MOVE SHARD

/// The re-placement primitive (SPEC 18.8.2): move shard `index` of the
/// newest version of `key` to `target`, or to a device chosen as a write
/// would choose. Copies from the source when it is reachable and intact,
/// otherwise rebuilds from the other shards; then writes the record at the
/// next revision to the new device and every other one; then removes
/// the source's copy if it can be reached.
async fn move_shard(
    node: &Arc<Node>,
    key: &str,
    shard_index: u8,
    target: Option<DeviceId>,
) -> Result<Response, Failure> {
    let record = newest_version(node, key).await?;
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let index = ShardIndex(shard_index);
    if !scheme.contains(index) {
        return Err(error(
            ErrorCode::ProtocolViolation,
            format!("{index} is outside scheme {}+{}", record.k, record.m),
        ));
    }
    let moved = move_shard_of_record(node, &record, index, target).await?;
    Ok(Response::MoveShard {
        record: moved.record,
        source: moved.source,
        source_cleaned: moved.source_cleaned,
        rebuilt: moved.rebuilt,
    })
}

/// What a re-placement did.
struct MovedShard {
    /// The record at its new revision.
    record: MetadataRecord,
    source: DeviceId,
    source_cleaned: bool,
    rebuilt: bool,
}

/// Re-place shard `index` of `record`, which must be the current record
/// of its version, onto `target` or a device chosen as a write would.
async fn move_shard_of_record(
    node: &Arc<Node>,
    record: &MetadataRecord,
    index: ShardIndex,
    target: Option<DeviceId>,
) -> Result<MovedShard, Failure> {
    let key = record.key.as_str();
    let shard_index = index.0;
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let source = record
        .device_for(index)
        .expect("validated record lists every index");
    let document = node.document();

    // Choose or check the destination. An explicit target needs only its
    // own node's view; an automatic choice needs the whole cluster's, so it
    // fails if any node is unreachable (fail-stop, 16.1).
    let shard_bytes = if record.size == 0 {
        0
    } else {
        shard_file_length(scheme, record.block_size, record.size)
            .ok_or_else(|| error(ErrorCode::Internal, "cannot size shard file"))?
    };
    let devices: Vec<DeviceId> = record.shards.iter().map(|s| s.device).collect();
    let eligible = |d: &DeviceStatus| {
        d.state == DeviceState::Active
            && d.available
            && d.free_bytes >= shard_bytes
            && !devices.contains(&d.device)
    };
    let destination = match target {
        Some(device) => {
            let owner = document.device(device).map(|d| d.node).ok_or_else(|| {
                Failure::Error(ErrorDetail {
                    device: Some(device),
                    ..ErrorDetail::new(
                        ErrorCode::DeviceUnavailable,
                        format!("{device} is not in the cluster document"),
                    )
                })
            })?;
            let mut connection = connect_to(node, owner).await?;
            let answer = connection
                .request(Request::LocalStatus)
                .await
                .map_err(|e| remote_failure(owner, e))?;
            let statuses = device_statuses(node, vec![(owner, answer)])?;
            match statuses.into_iter().find(|d| d.device == device) {
                Some(status) if eligible(&status) => status,
                _ => {
                    return Err(Failure::Error(ErrorDetail {
                        device: Some(device),
                        key: Some(key.to_string()),
                        version: Some(record.version),
                        ..ErrorDetail::new(
                            ErrorCode::InsufficientDevices,
                            format!("{device} is not an eligible destination: it must be active, have {shard_bytes} bytes free, and not already hold a shard of this version"),
                        )
                    }))
                }
            }
        }
        None => {
            let statuses = device_statuses(node, broadcast(node, Request::LocalStatus).await?)?;
            let mut ranked: Vec<DeviceStatus> = statuses.into_iter().filter(eligible).collect();
            ranked.sort_by(|a, b| {
                b.free_bytes
                    .cmp(&a.free_bytes)
                    .then(a.device.cmp(&b.device))
            });
            match ranked.into_iter().next() {
                Some(status) => status,
                None => {
                    return Err(Failure::Error(ErrorDetail {
                        key: Some(key.to_string()),
                        version: Some(record.version),
                        ..ErrorDetail::new(
                            ErrorCode::InsufficientDevices,
                            format!("no active device with {shard_bytes} bytes free that does not already hold a shard of this version"),
                        )
                    }))
                }
            }
        }
    };

    // Step 1: produce the shard on the destination.
    let mut rebuilt = false;
    if record.size > 0 {
        let copied = copy_shard(node, key, record, index, source, &destination).await;
        match copied {
            Ok(()) => {}
            Err(Failure::Error(detail)) => {
                tracing::info!(key, shard = shard_index, reason = %detail.message, "copy from source failed; rebuilding from the other shards");
                let geometry =
                    shard_geometry(scheme, record.block_size, record.size).ok_or_else(|| {
                        error(
                            ErrorCode::RecordsInconsistent,
                            "record has an impossible size",
                        )
                    })?;
                let sources = open_repair_sources(node, record, geometry.block_count).await?;
                rebuild_shards(node, key, record, &sources, &[(index, destination.device)]).await?;
                rebuilt = true;
            }
            Err(other) => return Err(other),
        }
    }

    // Steps 2 and 3: the record at the next revision, destination first.
    let mut next = record.clone();
    next.revision += 1;
    for shard in next.shards.iter_mut() {
        if shard.index == shard_index {
            shard.device = destination.device;
        }
    }
    let mut order: Vec<DeviceId> = vec![destination.device];
    order.extend(
        next.shards
            .iter()
            .map(|s| s.device)
            .filter(|d| *d != destination.device),
    );
    rewrite_record_copies(node, &next, &order).await?;

    // Step 4: the source's copy, if it can be reached.
    let source_cleaned = match document.device(source).map(|d| d.node) {
        Some(owner) => match connect_to(node, owner).await {
            Ok(mut connection) => connection
                .request(Request::DeleteVersion {
                    device: source,
                    key_hash: record.key_hash,
                    version: record.version,
                })
                .await
                .is_ok(),
            Err(_) => false,
        },
        None => false,
    };
    Ok(MovedShard {
        record: next,
        source,
        source_cleaned,
        rebuilt,
    })
}

// ----------------------------------------------------------------- DRAIN

/// One pass over a `draining` device (SPEC 18.2.1, 18.2.2): list the
/// versions on it once, estimate, then re-place each shard exactly once,
/// reporting every outcome. Finishes when the list is exhausted whatever
/// happened to individual versions, so it cannot loop.
async fn drain(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    device: DeviceId,
    partial: bool,
) -> Result<(), Failure> {
    let document = node.document();
    let Some(entry) = document.device(device) else {
        let detail = ErrorDetail {
            device: Some(device),
            ..ErrorDetail::new(
                ErrorCode::DeviceUnavailable,
                format!("{device} is not in the cluster document"),
            )
        };
        return respond(writer, id, Err(Failure::Error(detail))).await;
    };
    if entry.state != DeviceState::Draining {
        let detail = ErrorDetail {
            device: Some(device),
            ..ErrorDetail::new(
                ErrorCode::ProtocolViolation,
                format!(
                    "{device} is {}, not draining; `djbod cluster set-state {} draining` first",
                    format!("{:?}", entry.state).to_lowercase(),
                    device.0
                ),
            )
        };
        return respond(writer, id, Err(Failure::Error(detail))).await;
    }
    let owner = entry.node;
    let records = match fetch_device_records(node, owner, device).await {
        Ok(records) => records,
        Err(f) => return respond(writer, id, Err(f)).await,
    };
    let statuses = match broadcast(node, Request::LocalStatus).await {
        Ok(answers) => match device_statuses(node, answers) {
            Ok(statuses) => statuses,
            Err(f) => return respond(writer, id, Err(f)).await,
        },
        Err(f) => return respond(writer, id, Err(f)).await,
    };
    respond(writer, id, Ok(Response::DrainStarted)).await?;
    let mut sequence: u64 = 0;

    // The estimate (18.2.2).
    let required_devices = document.k as u64 + document.m as u64;
    let active: Vec<&DeviceStatus> = statuses
        .iter()
        .filter(|d| d.state == DeviceState::Active && d.available)
        .collect();
    let target_free_bytes: u64 = active.iter().map(|d| d.free_bytes).sum();
    let mut shard_bytes: u64 = 0;
    for record in &records {
        if let Ok(scheme) = record.scheme() {
            shard_bytes += shard_file_length(scheme, record.block_size, record.size).unwrap_or(0);
        }
    }
    send_event(
        writer,
        id,
        &mut sequence,
        &DrainEvent::Estimate {
            device,
            node: owner,
            versions: records.len() as u64,
            shard_bytes,
            target_free_bytes,
            active_devices: active.len() as u64,
            required_devices,
        },
    )
    .await?;
    let shortfall = if (active.len() as u64) < required_devices {
        Some(format!(
            "{} active device(s), but every version needs {required_devices}; no version has a legal target",
            active.len()
        ))
    } else if target_free_bytes < shard_bytes {
        Some(format!(
            "{shard_bytes} bytes to move but active devices have {target_free_bytes} bytes free within headroom"
        ))
    } else {
        None
    };
    if let Some(reason) = shortfall {
        if !partial {
            let end = StreamEnd::failed(ErrorDetail {
                device: Some(device),
                ..ErrorDetail::new(
                    ErrorCode::InsufficientDevices,
                    format!("{reason}; add capacity, or pass --partial to move what fits"),
                )
            });
            write_message(writer, &Message::EndOfStream { id, end }).await?;
            return Ok(());
        }
    }

    // The pass.
    let total = records.len();
    let mut skipped = 0usize;
    for record in &records {
        let event = match drain_one(node, device, record).await {
            Ok(DrainOutcome::Moved(moved)) => DrainEvent::Moved {
                key: record.key.clone(),
                version: record.version,
                shard_index: moved.shard_index,
                destination: moved.destination,
                rebuilt: moved.rebuilt,
            },
            Ok(DrainOutcome::Deleted) => DrainEvent::Deleted {
                key: record.key.clone(),
                version: record.version,
            },
            Err(Failure::Error(detail)) => {
                skipped += 1;
                DrainEvent::Skipped {
                    key: record.key.clone(),
                    version: record.version,
                    detail,
                }
            }
            Err(other) => return Err(other),
        };
        match &event {
            DrainEvent::Moved {
                key, destination, ..
            } => tracing::info!(key, %device, %destination, "drain: shard moved"),
            DrainEvent::Skipped { key, detail, .. } => {
                tracing::warn!(key, %device, reason = %detail.message, "drain: version skipped")
            }
            DrainEvent::Deleted { key, .. } => {
                tracing::info!(key, %device, "drain: version deleted since the pass began")
            }
            DrainEvent::Estimate { .. } => {}
        }
        send_event(writer, id, &mut sequence, &event).await?;
    }
    let end = if skipped > 0 {
        StreamEnd::failed(ErrorDetail {
            device: Some(device),
            ..ErrorDetail::new(
                ErrorCode::WriteFailed,
                format!(
                    "{skipped} of {total} version(s) could not be moved; the device stays draining and serves what it holds"
                ),
            )
        })
    } else {
        StreamEnd::ok()
    };
    write_message(writer, &Message::EndOfStream { id, end }).await?;
    Ok(())
}

/// Every record on `device`, asked of its owning node.
async fn fetch_device_records(
    node: &Arc<Node>,
    owner: NodeId,
    device: DeviceId,
) -> Result<Vec<MetadataRecord>, Failure> {
    let mut connection = connect_to(node, owner).await?;
    let mut all = Vec::new();
    let mut after: Option<RecordCursor> = None;
    loop {
        let answer = connection
            .request(Request::LocalRecords {
                device,
                after: after.clone(),
            })
            .await
            .map_err(|e| remote_failure(owner, e))?;
        match answer {
            Response::LocalRecords { records, truncated } => {
                after = records.last().map(|r| RecordCursor {
                    key_hash: r.record.key_hash,
                    version: r.record.version,
                });
                all.extend(records.into_iter().map(|r| r.record));
                if !truncated || after.is_none() {
                    return Ok(all);
                }
            }
            other => {
                return Err(error(
                    ErrorCode::ProtocolViolation,
                    format!("{owner} answered LocalRecords with {other:?}"),
                ))
            }
        }
    }
}

struct DrainedShard {
    shard_index: u8,
    destination: DeviceId,
    rebuilt: bool,
}

enum DrainOutcome {
    Moved(DrainedShard),
    /// No copy of the version's record exists anywhere any more.
    Deleted,
}

/// Re-place the shard that `copy`, a record found on `device`, says the
/// device holds. The version's current record decides: a copy the
/// current record does not agree with is a stale leftover, reported and
/// left for `scrub --repair`.
async fn drain_one(
    node: &Arc<Node>,
    device: DeviceId,
    copy: &MetadataRecord,
) -> Result<DrainOutcome, Failure> {
    let located = lookup(node, copy.key_hash).await?;
    // The pass listed this version some time ago. If no copy of its
    // record is left anywhere, the object was deleted or replaced in the
    // meantime (12, 14), the copy here went with it, and there is nothing
    // to move. Checked before `versions_of`, which would otherwise report
    // an unrelated inconsistency for a version it cannot see.
    if !located.iter().any(|c| c.record.version == copy.version) {
        return Ok(DrainOutcome::Deleted);
    }
    let versions = versions_of(&copy.key, located)?;
    let index = shard_to_drain(device, copy, &versions)?;
    let current = versions
        .iter()
        .find(|v| v.version == copy.version)
        .expect("checked by shard_to_drain");
    let moved = move_shard_of_record(node, current, index, None).await?;
    Ok(DrainOutcome::Moved(DrainedShard {
        shard_index: index.0,
        destination: moved
            .record
            .device_for(index)
            .expect("the new record lists every index"),
        rebuilt: moved.rebuilt,
    }))
}

/// Which shard of `copy`'s version the current record places on
/// `device`. Copies exist for the version (the caller checked), so a
/// current record that does not place a shard here, or no current record
/// at all, means the copy on the device is a stale leftover of a
/// re-placement (18.8.1), which is the scrub's to remove.
fn shard_to_drain(
    device: DeviceId,
    copy: &MetadataRecord,
    versions: &[MetadataRecord],
) -> Result<ShardIndex, Failure> {
    let stale = || {
        Failure::Error(ErrorDetail {
            device: Some(device),
            key: Some(copy.key.clone()),
            version: Some(copy.version),
            ..ErrorDetail::new(
                ErrorCode::RecordsInconsistent,
                "stale copy: the current record of this version does not place a shard here; `djbod scrub --repair` removes it",
            )
        })
    };
    let current = versions
        .iter()
        .find(|v| v.version == copy.version)
        .ok_or_else(stale)?;
    current.shard_on(device).ok_or_else(stale)
}

/// Stream shard `index` from `source` to `destination`, verifying every
/// block in transit. Any failure leaves nothing on the destination.
async fn copy_shard(
    node: &Arc<Node>,
    key: &str,
    record: &MetadataRecord,
    index: ShardIndex,
    source: DeviceId,
    destination: &DeviceStatus,
) -> Result<(), Failure> {
    let scheme = record
        .scheme()
        .map_err(|e| error(ErrorCode::RecordsInconsistent, e.to_string()))?;
    let geometry = shard_geometry(scheme, record.block_size, record.size).ok_or_else(|| {
        error(
            ErrorCode::RecordsInconsistent,
            "record has an impossible size",
        )
    })?;
    let document = node.document();
    let source_owner = document.device(source).map(|d| d.node).ok_or_else(|| {
        error(
            ErrorCode::DeviceUnavailable,
            format!("{source} is not in the cluster document"),
        )
    })?;
    let mut from = connect_to(node, source_owner).await?;
    let read_id = from
        .send_request(Request::GetShard {
            device: source,
            key_hash: record.key_hash,
            version: record.version,
            shard_index: index.0,
            first_block: 0,
            block_count: geometry.block_count,
        })
        .await
        .map_err(|e| remote_failure(source_owner, e))?;
    match from.read_response(read_id).await {
        Ok(Response::GetShard { .. }) => {}
        Ok(other) => {
            return Err(error(
                ErrorCode::ProtocolViolation,
                format!("{source_owner} answered GetShard with {other:?}"),
            ))
        }
        Err(e) => return Err(remote_failure(source_owner, e)),
    }
    let mut to = connect_to(node, destination.node).await?;
    let write_id = to
        .send_request(Request::PutShard {
            device: destination.device,
            key_hash: record.key_hash,
            version: record.version,
            shard_index: index.0,
            k: scheme.data_shards() as u8,
            m: scheme.parity_shards() as u8,
            block_length: record.block_size,
            object_size: record.size,
        })
        .await
        .map_err(|e| remote_failure(destination.node, e))?;
    match to.read_response(write_id).await {
        Ok(Response::PutShardReady) => {}
        Ok(other) => {
            return Err(error(
                ErrorCode::ProtocolViolation,
                format!("{} answered PutShard with {other:?}", destination.node),
            ))
        }
        Err(e) => return Err(remote_failure(destination.node, e)),
    }
    let relayed = relay_shard_blocks(
        &mut from,
        read_id,
        &mut to,
        write_id,
        key,
        record,
        index,
        source,
        source_owner,
        destination.node,
        geometry.block_count,
    )
    .await;
    if let Err(failure) = relayed {
        // Tell the destination to drop its temporary and wait for it to
        // say so, so that a rebuild can begin writing the same shard.
        let detail = match &failure {
            Failure::Error(detail) => detail.clone(),
            _ => ErrorDetail::new(ErrorCode::Internal, "copy abandoned".to_string()),
        };
        let _ = to
            .send_end(
                write_id,
                StreamEnd {
                    error: Some(detail),
                    object_size: None,
                    object_checksum: None,
                    reconstructed: Vec::new(),
                    missing_records: Vec::new(),
                },
            )
            .await;
        let _ = to.read_response(write_id).await;
        return Err(failure);
    }
    match to.read_response(write_id).await {
        Ok(Response::PutShardDone) => Ok(()),
        Ok(other) => Err(error(
            ErrorCode::ProtocolViolation,
            format!("{} ended PutShard with {other:?}", destination.node),
        )),
        Err(e) => Err(remote_failure(destination.node, e)),
    }
}

/// Forward every block of the source stream to the destination stream,
/// checking each block's checksum on the way, then end the destination
/// stream. Returns without reading the destination's final answer.
#[allow(clippy::too_many_arguments)]
async fn relay_shard_blocks(
    from: &mut Connection,
    read_id: u32,
    to: &mut Connection,
    write_id: u32,
    key: &str,
    record: &MetadataRecord,
    index: ShardIndex,
    source: DeviceId,
    source_owner: NodeId,
    destination_node: NodeId,
    block_count: u64,
) -> Result<(), Failure> {
    for stripe in 0..block_count {
        let block = match from.read_stream_item(read_id).await {
            Ok(StreamItem::Data(data)) if data.sequence == stripe => data,
            Ok(other) => {
                return Err(Failure::Error(ErrorDetail {
                    device: Some(source),
                    key: Some(key.to_string()),
                    version: Some(record.version),
                    shard_index: Some(index.0),
                    stripe: Some(stripe),
                    ..ErrorDetail::new(
                        ErrorCode::BlockChecksumMismatch,
                        format!("source stream ended or misordered at stripe {stripe}: {other:?}"),
                    )
                }))
            }
            Err(e) => return Err(remote_failure(source_owner, e)),
        };
        if checksum_block(&block.bytes) != block.checksum {
            return Err(Failure::Error(ErrorDetail {
                device: Some(source),
                key: Some(key.to_string()),
                version: Some(record.version),
                shard_index: Some(index.0),
                stripe: Some(stripe),
                ..ErrorDetail::new(
                    ErrorCode::BlockChecksumMismatch,
                    format!("source block {stripe} fails its checksum"),
                )
            }));
        }
        to.send_data(write_id, block)
            .await
            .map_err(|e| remote_failure(destination_node, e))?;
    }
    match from.read_stream_item(read_id).await {
        Ok(StreamItem::End(end)) if end.error.is_none() => {}
        other => {
            return Err(error(
                ErrorCode::ProtocolViolation,
                format!("source did not end its stream cleanly: {other:?}"),
            ))
        }
    }
    to.send_end(
        write_id,
        StreamEnd {
            error: None,
            object_size: Some(record.size),
            object_checksum: Some(record.object_checksum),
            reconstructed: Vec::new(),
            missing_records: Vec::new(),
        },
    )
    .await
    .map_err(|e| remote_failure(destination_node, e))?;
    Ok(())
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
                            "node sent block {} when {stripe} was expected",
                            data.sequence
                        ),
                    )
                }))
            }
            Ok(StreamItem::End(end)) => {
                // A device whose file fails part way (a read error) ends
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
                        format!("node did not end its stream cleanly: {other:?}"),
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

// ---------------------------------------------------------------- SCRUB

/// Write one scrub or drain event to the client as a CBOR data frame.
async fn send_event<E: serde::Serialize>(
    writer: &mut Writer,
    id: u32,
    sequence: &mut u64,
    event: &E,
) -> Result<(), Failure> {
    let bytes = djbod_proto::codec::encode_cbor(event)
        .map_err(|e| error(ErrorCode::Internal, e.to_string()))?;
    write_message(
        writer,
        &Message::Data {
            id,
            data: DataFrame {
                sequence: *sequence,
                checksum: checksum_block(&bytes),
                bytes,
            },
        },
    )
    .await?;
    *sequence += 1;
    Ok(())
}

/// The cluster-wide scrub (SPEC 20.1.2): every node's local scrub, run
/// concurrently and relayed as it happens; then the cross-node checks;
/// then, if asked, one repair per damaged key.
async fn scrub(
    node: &Arc<Node>,
    id: u32,
    writer: &mut Writer,
    max_bytes_per_second: Option<u64>,
    repair: bool,
) -> Result<(), Failure> {
    respond(writer, id, Ok(Response::ScrubStarted)).await?;
    let mut sequence: u64 = 0;
    let mut damaged_keys: BTreeSet<String> = std::collections::BTreeSet::new();
    // Shards the nodes' own scrubs found damaged, for the availability
    // count of the cross-node phase (20.1.2.2).
    let mut damaged_shards: BTreeSet<(VersionId, u8)> = BTreeSet::new();
    let mut failed_nodes = 0usize;
    let mut finding_count = 0usize;

    // Phase 1: every node's local scrub, concurrently, relayed as events
    // arrive over a channel.
    let document = node.document();
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<ScrubEvent>(256);
    let mut tasks = JoinSet::new();
    for entry in &document.nodes {
        let target = entry.id;
        let node = node.clone();
        let sender = sender.clone();
        tasks.spawn(async move {
            if let Err(f) = relay_local_scrub(&node, target, max_bytes_per_second, &sender).await {
                let detail = match f {
                    Failure::Error(detail) => detail,
                    Failure::Close(end) => {
                        ErrorDetail::new(ErrorCode::NodeUnreachable, format!("{end:?}"))
                    }
                };
                let _ = sender
                    .send(ScrubEvent::NodeFailed {
                        node: target,
                        detail,
                    })
                    .await;
            }
        });
    }
    drop(sender);
    while let Some(event) = receiver.recv().await {
        match &event {
            ScrubEvent::NodeFinding { finding, .. } => {
                finding_count += 1;
                if let Some(key) = finding.repair_key() {
                    damaged_keys.insert(key.to_string());
                }
                match finding {
                    djbod_core::scrub::Finding::ShardUnreadable {
                        version,
                        shard_index,
                        ..
                    }
                    | djbod_core::scrub::Finding::ShardBlocksCorrupt {
                        version,
                        shard_index,
                        ..
                    } => {
                        damaged_shards.insert((*version, *shard_index));
                    }
                    _ => {}
                }
            }
            ScrubEvent::NodeFailed { .. } => failed_nodes += 1,
            _ => {}
        }
        send_event(writer, id, &mut sequence, &event).await?;
    }
    while tasks.join_next().await.is_some() {}

    // Phase 2: the cross-node checks, a merge of one record stream per
    // device (20.1.2.2), relayed as its events arrive over a channel.
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<ScrubEvent>(256);
    let merge = {
        let node = node.clone();
        tokio::spawn(async move { cross_check(&node, sender, damaged_shards).await })
    };
    while let Some(event) = receiver.recv().await {
        send_event(writer, id, &mut sequence, &event).await?;
    }
    let outcome = merge.await.map_err(|join| {
        error(
            ErrorCode::Internal,
            format!("cross-node check task failed: {join}"),
        )
    })?;
    finding_count += outcome.findings;
    damaged_keys.extend(outcome.damaged_keys);
    let check_stopped: Option<u64> = outcome.stopped_after;

    // Phase 3: repairs, one per damaged key, from this one place.
    let mut repair_failures = 0usize;
    if repair {
        for key in &damaged_keys {
            let event = match repair_object(node, key).await {
                Ok(Response::RepairObject(report)) => ScrubEvent::Repaired {
                    key: key.clone(),
                    report,
                },
                Ok(other) => ScrubEvent::RepairFailed {
                    key: key.clone(),
                    detail: ErrorDetail::new(ErrorCode::Internal, format!("unexpected {other:?}")),
                },
                Err(Failure::Error(detail)) => {
                    repair_failures += 1;
                    ScrubEvent::RepairFailed {
                        key: key.clone(),
                        detail,
                    }
                }
                Err(other) => return Err(other),
            };
            send_event(writer, id, &mut sequence, &event).await?;
        }
    }

    let end = if failed_nodes > 0 || check_stopped.is_some() {
        let checks = match check_stopped {
            Some(checked) => format!(
                "the cross-node checks stopped after {checked} version(s), the rest unchecked"
            ),
            None => "every version was checked".to_string(),
        };
        StreamEnd::failed(ErrorDetail::new(
            ErrorCode::NodeUnreachable,
            format!(
                "{failed_nodes} node(s) could not be scrubbed; {checks}; {finding_count} finding(s) where checks ran"
            ),
        ))
    } else if repair_failures > 0 {
        StreamEnd::failed(ErrorDetail::new(
            ErrorCode::WriteFailed,
            format!("{repair_failures} repair(s) failed"),
        ))
    } else {
        StreamEnd::ok()
    };
    write_message(writer, &Message::EndOfStream { id, end }).await?;
    Ok(())
}

/// Run one node's `LocalScrub` and forward its items as events.
async fn relay_local_scrub(
    node: &Arc<Node>,
    target: NodeId,
    max_bytes_per_second: Option<u64>,
    sender: &tokio::sync::mpsc::Sender<ScrubEvent>,
) -> Result<(), Failure> {
    let mut connection = connect_to(node, target).await?;
    let request_id = connection
        .send_request(Request::LocalScrub {
            max_bytes_per_second,
        })
        .await
        .map_err(|e| remote_failure(target, e))?;
    match connection.read_response(request_id).await {
        Ok(Response::LocalScrubStarted) => {}
        Ok(other) => {
            return Err(error(
                ErrorCode::ProtocolViolation,
                format!("{target} answered LocalScrub with {other:?}"),
            ))
        }
        Err(e) => return Err(remote_failure(target, e)),
    }
    loop {
        match connection.read_stream_item(request_id).await {
            Ok(StreamItem::Data(data)) => {
                if checksum_block(&data.bytes) != data.checksum {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        "scrub item corrupt in transit",
                    ));
                }
                let item: ScrubItem = djbod_proto::codec::decode_cbor(&data.bytes)
                    .map_err(|e| error(ErrorCode::ProtocolViolation, e.to_string()))?;
                let event = match item {
                    ScrubItem::Finding { device, finding } => ScrubEvent::NodeFinding {
                        node: target,
                        device,
                        finding,
                    },
                    ScrubItem::Summary { device, summary } => ScrubEvent::NodeSummary {
                        node: target,
                        device,
                        summary,
                    },
                };
                if sender.send(event).await.is_err() {
                    return Ok(());
                }
            }
            Ok(StreamItem::End(end)) => {
                return match end.error {
                    None => Ok(()),
                    Some(detail) => Err(Failure::Error(ErrorDetail {
                        node: Some(target),
                        ..detail
                    })),
                }
            }
            Err(e) => return Err(remote_failure(target, e)),
        }
    }
}

/// The checks only the coordinator can make for one key: record copies
/// complete and agreeing, and every listed device actually holding its
/// shard file.
/// A node the cross-node check could not use: unreachable, refusing, or
/// answering out of protocol, or a device stream that ended in error.
/// Every version from that point on is unchecked.
struct Unreachable {
    node: NodeId,
    detail: ErrorDetail,
}

impl Unreachable {
    fn from_failure(node: NodeId, failure: Failure) -> Unreachable {
        let detail = match failure {
            Failure::Error(detail) => detail,
            Failure::Close(end) => ErrorDetail::new(
                ErrorCode::NodeUnreachable,
                format!("the connection ended: {end:?}"),
            ),
        };
        Unreachable {
            node: detail.node.unwrap_or(node),
            detail,
        }
    }
}

/// One record as a device stream yields it.
struct StreamedRecord {
    device: DeviceId,
    record: MetadataRecord,
    shard_present: bool,
}

/// One device's `LocalRecords` stream: a connection held for the whole
/// cross-node phase and pages fetched as the merge consumes them.
struct DeviceStream {
    owner: NodeId,
    device: DeviceId,
    connection: Connection,
    page: std::collections::VecDeque<DeviceRecord>,
    after: Option<RecordCursor>,
    exhausted: bool,
}

impl DeviceStream {
    async fn open(node: &Node, owner: NodeId, device: DeviceId) -> Result<DeviceStream, Failure> {
        Ok(DeviceStream {
            owner,
            device,
            connection: connect_to(node, owner).await?,
            page: std::collections::VecDeque::new(),
            after: None,
            exhausted: false,
        })
    }

    async fn next(&mut self) -> Result<Option<StreamedRecord>, Failure> {
        if self.page.is_empty() && !self.exhausted {
            let answer = self
                .connection
                .request(Request::LocalRecords {
                    device: self.device,
                    after: self.after.clone(),
                })
                .await
                .map_err(|e| remote_failure(self.owner, e))?;
            match answer {
                Response::LocalRecords { records, truncated } => {
                    self.after = records.last().map(|r| RecordCursor {
                        key_hash: r.record.key_hash,
                        version: r.record.version,
                    });
                    self.exhausted = !truncated || records.is_empty();
                    self.page = records.into();
                }
                other => {
                    return Err(error(
                        ErrorCode::ProtocolViolation,
                        format!("{} answered LocalRecords with {other:?}", self.owner),
                    ))
                }
            }
        }
        Ok(self.page.pop_front().map(|r| StreamedRecord {
            device: self.device,
            record: r.record,
            shard_present: r.shard_present,
        }))
    }
}

/// A source of one device's records for the merge: the device's stream,
/// or, in tests, a fixed sequence that may end in a failure.
enum RecordSource {
    Device(Box<DeviceStream>),
    #[cfg(test)]
    Fixed {
        owner: NodeId,
        device: DeviceId,
        items: std::collections::VecDeque<Result<StreamedRecord, ErrorDetail>>,
    },
}

impl RecordSource {
    fn owner(&self) -> NodeId {
        match self {
            RecordSource::Device(stream) => stream.owner,
            #[cfg(test)]
            RecordSource::Fixed { owner, .. } => *owner,
        }
    }

    fn device(&self) -> DeviceId {
        match self {
            RecordSource::Device(stream) => stream.device,
            #[cfg(test)]
            RecordSource::Fixed { device, .. } => *device,
        }
    }

    async fn next(&mut self) -> Result<Option<StreamedRecord>, Failure> {
        match self {
            RecordSource::Device(stream) => stream.next().await,
            #[cfg(test)]
            RecordSource::Fixed { items, .. } => match items.pop_front() {
                None => Ok(None),
                Some(Ok(item)) => Ok(Some(item)),
                Some(Err(detail)) => Err(Failure::Error(detail)),
            },
        }
    }
}

/// What the cross-node phase found and where it got to.
#[derive(Default)]
struct CrossCheckOutcome {
    findings: usize,
    damaged_keys: BTreeSet<String>,
    versions_checked: u64,
    /// `Some(n)` when a stream failed after `n` versions were checked.
    stopped_after: Option<u64>,
    exposure: Exposure,
    /// Versions by (shards total, shards available, k).
    availability: BTreeMap<(u8, u8, u8), u64>,
}

impl CrossCheckOutcome {
    /// The count of every version checked by its shards available, for
    /// the end of the phase (20.1.2.2). Whole versions first.
    fn availability_event(&self) -> ScrubEvent {
        let mut versions: Vec<ShardAvailability> = self
            .availability
            .iter()
            .map(|((total, available, k), count)| ShardAvailability {
                shards_total: *total,
                shards_available: *available,
                k: *k,
                versions: *count,
            })
            .collect();
        versions.sort_by_key(|v| (v.shards_total, std::cmp::Reverse(v.shards_available)));
        ScrubEvent::CrossCheckAvailability {
            versions_checked: self.versions_checked,
            versions,
        }
    }
}

/// How many of one version's shards are available, and of how many.
struct ShardCount {
    total: u8,
    available: u8,
    k: u8,
}

/// What the devices the merge could not read (5.6) cost, counted from
/// the records of every version checked (20.1.2.2): not damage, since
/// nothing is known to be wrong with a shard on such a device, but the
/// versions that can be read only by going around it, or not at all.
#[derive(Default)]
struct Exposure {
    /// Checked versions with a shard on each unread device.
    by_device: BTreeMap<DeviceId, u64>,
    /// Versions with at least one shard on an unread device.
    with_shards_out: u64,
    /// Of those, with exactly m out: one further loss makes them unreadable.
    at_the_limit: u64,
    /// With more than m out: unreadable until a device returns.
    unreadable: u64,
}

impl Exposure {
    /// Count one version from its record and the devices not read.
    fn note(&mut self, record: &MetadataRecord, unread: &BTreeSet<DeviceId>) {
        let mut out = 0usize;
        for shard in &record.shards {
            if unread.contains(&shard.device) {
                out += 1;
                *self.by_device.entry(shard.device).or_default() += 1;
            }
        }
        if out == 0 {
            return;
        }
        self.with_shards_out += 1;
        if out == record.m as usize {
            self.at_the_limit += 1;
        } else if out > record.m as usize {
            self.unreadable += 1;
        }
    }

    /// The event for the end of the phase, when any device was unread.
    fn event(&self, unread: &BTreeSet<DeviceId>, versions_checked: u64) -> Option<ScrubEvent> {
        if unread.is_empty() {
            return None;
        }
        Some(ScrubEvent::CrossCheckExposure {
            unread: unread
                .iter()
                .map(|device| DeviceExposure {
                    device: *device,
                    versions: self.by_device.get(device).copied().unwrap_or(0),
                })
                .collect(),
            versions_checked,
            versions_with_shards_out: self.with_shards_out,
            versions_at_the_limit: self.at_the_limit,
            versions_unreadable: self.unreadable,
        })
    }
}

/// How often the merge reports where it is (20.1.2.2).
const PROGRESS_EVERY_VERSIONS: u64 = 10_000;

/// The cross-node checks (20.1.2.2): open one record stream per device
/// in the document, all at once, and merge them.
async fn cross_check(
    node: &Arc<Node>,
    events: tokio::sync::mpsc::Sender<ScrubEvent>,
    damaged_shards: BTreeSet<(VersionId, u8)>,
) -> CrossCheckOutcome {
    let document = node.document();
    let mut sources = Vec::new();
    for entry in document
        .devices
        .iter()
        .filter(|d| d.state != DeviceState::Removed)
    {
        match DeviceStream::open(node, entry.node, entry.id).await {
            Ok(stream) => sources.push(RecordSource::Device(Box::new(stream))),
            Err(failure) => {
                let mut outcome = CrossCheckOutcome::default();
                stop(
                    &events,
                    &mut outcome,
                    Unreachable::from_failure(entry.node, failure),
                )
                .await;
                return outcome;
            }
        }
    }
    merge_sources(sources, events, &damaged_shards).await
}

/// Merge the sources in (key hash, version) order; each group of equal
/// heads is one version's copies across the cluster, checked from the
/// group alone. A source that fails ends the merge after the group in
/// hand; an event the client no longer reads ends it silently.
async fn merge_sources(
    mut sources: Vec<RecordSource>,
    events: tokio::sync::mpsc::Sender<ScrubEvent>,
    damaged_shards: &BTreeSet<(VersionId, u8)>,
) -> CrossCheckOutcome {
    let mut outcome = CrossCheckOutcome::default();
    // Record streams that failed because their device could not be read
    // (5.6). Unlike any other failure, this does not stop the merge: the
    // node's own scrub has reported the device once, and from here on
    // the groups are checked without expecting a copy from it. A device
    // that fails part way joins the set at that point; the groups before
    // it were checked with its copies present.
    let mut unread: BTreeSet<DeviceId> = BTreeSet::new();
    // The devices with a stream: a listed device outside this set, and
    // not unread, is removed or gone from the document, and a shard the
    // record places on it is lost (18.3), not a copy that went missing.
    let known: BTreeSet<DeviceId> = sources.iter().map(|s| s.device()).collect();
    let mut heads: Vec<Option<StreamedRecord>> = Vec::with_capacity(sources.len());
    for source in sources.iter_mut() {
        match source.next().await {
            Ok(head) => heads.push(head),
            Err(Failure::Error(detail)) if detail.code == ErrorCode::DeviceUnavailable => {
                unread.insert(source.device());
                heads.push(None);
            }
            Err(failure) => {
                let unreachable = Unreachable::from_failure(source.owner(), failure);
                stop(&events, &mut outcome, unreachable).await;
                return outcome;
            }
        }
    }
    loop {
        let Some(smallest) = heads
            .iter()
            .flatten()
            .map(|h| (h.record.key_hash, h.record.version))
            .min()
        else {
            let _ = events.send(outcome.availability_event()).await;
            if let Some(exposure) = outcome.exposure.event(&unread, outcome.versions_checked) {
                let _ = events.send(exposure).await;
            }
            return outcome;
        };
        let mut group: Vec<LocatedRecord> = Vec::new();
        let mut present: BTreeMap<DeviceId, bool> = BTreeMap::new();
        let mut taken: Vec<usize> = Vec::new();
        for (index, head) in heads.iter_mut().enumerate() {
            let matches = head
                .as_ref()
                .is_some_and(|h| (h.record.key_hash, h.record.version) == smallest);
            if matches {
                let item = head.take().expect("matched");
                present.insert(item.device, item.shard_present);
                group.push(LocatedRecord {
                    device: item.device,
                    record: item.record,
                });
                taken.push(index);
            }
        }
        // What the unread devices cost this version (5.6), from the copy
        // at the highest revision, whatever the checks below conclude.
        if let Some(current) = group.iter().max_by_key(|c| c.record.revision) {
            outcome.exposure.note(&current.record, &unread);
        }
        let (findings, shards) = check_group(group, &present, &unread, &known, damaged_shards);
        if let Some(shards) = shards {
            *outcome
                .availability
                .entry((shards.total, shards.available, shards.k))
                .or_default() += 1;
        }
        for finding in findings {
            outcome.findings += 1;
            outcome
                .damaged_keys
                .insert(finding_key(&finding).to_string());
            if events
                .send(ScrubEvent::ClusterFinding(finding))
                .await
                .is_err()
            {
                return outcome;
            }
        }
        outcome.versions_checked += 1;
        if outcome.versions_checked % PROGRESS_EVERY_VERSIONS == 0 {
            let progress = ScrubEvent::CrossCheckProgress {
                versions_checked: outcome.versions_checked,
                key_hash: smallest.0,
            };
            if events.send(progress).await.is_err() {
                return outcome;
            }
        }
        for index in taken {
            match sources[index].next().await {
                Ok(head) => heads[index] = head,
                Err(Failure::Error(detail)) if detail.code == ErrorCode::DeviceUnavailable => {
                    unread.insert(sources[index].device());
                    heads[index] = None;
                }
                Err(failure) => {
                    let unreachable = Unreachable::from_failure(sources[index].owner(), failure);
                    stop(&events, &mut outcome, unreachable).await;
                    // What was counted before the stop still stands.
                    let _ = events.send(outcome.availability_event()).await;
                    if let Some(exposure) =
                        outcome.exposure.event(&unread, outcome.versions_checked)
                    {
                        let _ = events.send(exposure).await;
                    }
                    return outcome;
                }
            }
        }
    }
}

async fn stop(
    events: &tokio::sync::mpsc::Sender<ScrubEvent>,
    outcome: &mut CrossCheckOutcome,
    unreachable: Unreachable,
) {
    outcome.stopped_after = Some(outcome.versions_checked);
    let _ = events
        .send(ScrubEvent::CrossCheckStopped {
            node: unreachable.node,
            detail: unreachable.detail,
            versions_checked: outcome.versions_checked,
            versions_unchecked: None,
        })
        .await;
}

/// The checks for one version from its copies (20.1.2.2): that the
/// copies of the current revision exist and agree, that no stale copy
/// remains, that every listed device also has the shard file, and that
/// no shard sits on a device the cluster has given up (18.2.1, 18.3).
/// Also how many of the version's shards are available, counting a
/// shard on an unread or lost device, a missing file, or one the node's
/// own scrub found damaged as not; `None` when the copies could not be
/// trusted, since then the version's shards are not known.
fn check_group(
    group: Vec<LocatedRecord>,
    present: &BTreeMap<DeviceId, bool>,
    unread: &BTreeSet<DeviceId>,
    known: &BTreeSet<DeviceId>,
    damaged_shards: &BTreeSet<(VersionId, u8)>,
) -> (Vec<ClusterFinding>, Option<ShardCount>) {
    let mut findings = Vec::new();
    let Some(first) = group.first() else {
        return (findings, None);
    };
    let key = first.record.key.clone();
    // A listed device with no copy here, no stream, and not unread is
    // removed or gone from the document: its shard is lost, which is a
    // finding of its own below, not a copy count that came up short.
    let lost: BTreeSet<DeviceId> = group
        .iter()
        .flat_map(|copy| copy.record.shards.iter().map(|s| s.device))
        .filter(|device| {
            !known.contains(device)
                && !unread.contains(device)
                && !group.iter().any(|copy| copy.device == *device)
        })
        .collect();
    let not_expected: BTreeSet<DeviceId> = unread.union(&lost).copied().collect();
    let versions = match versions_of_excluding(&key, group.clone(), &not_expected) {
        Ok(versions) => versions,
        Err(Failure::Error(detail)) => {
            findings.push(ClusterFinding::RecordsInconsistent {
                key,
                version: detail.version,
                detail: detail.message,
            });
            return (findings, None);
        }
        Err(_) => return (findings, None),
    };
    // The group is one version, so this is its one trusted record.
    let shards = versions.first().map(|record| {
        let unavailable = record
            .shards
            .iter()
            .filter(|shard| {
                lost.contains(&shard.device)
                    || unread.contains(&shard.device)
                    || present.get(&shard.device) == Some(&false)
                    || damaged_shards.contains(&(record.version, shard.index))
            })
            .count();
        let total = record.k + record.m;
        ShardCount {
            total,
            available: total.saturating_sub(unavailable as u8),
            k: record.k,
        }
    });
    for record in &versions {
        for (device, revision) in stale_copies(record, &group) {
            findings.push(ClusterFinding::StaleCopy {
                key: key.clone(),
                version: record.version,
                device,
                revision,
                current_revision: record.revision,
            });
        }
    }
    for record in versions {
        for shard in &record.shards {
            if lost.contains(&shard.device) {
                findings.push(ClusterFinding::ShardLost {
                    key: key.clone(),
                    version: record.version,
                    device: shard.device,
                    shard_index: shard.index,
                });
                continue;
            }
            // Every other listed device has a copy or is unread; the
            // question is only whether it also has the shard file, which
            // an empty object never has.
            if record.size > 0 && present.get(&shard.device) == Some(&false) {
                findings.push(ClusterFinding::ShardMissingOnDevice {
                    key: key.clone(),
                    version: record.version,
                    device: shard.device,
                    shard_index: shard.index,
                });
            }
        }
    }
    (findings, shards)
}

fn finding_key(finding: &ClusterFinding) -> &str {
    match finding {
        ClusterFinding::RecordsInconsistent { key, .. }
        | ClusterFinding::ShardMissingOnDevice { key, .. }
        | ClusterFinding::ShardLost { key, .. }
        | ClusterFinding::StaleCopy { key, .. } => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djbod_core::record::{ShardLocation, RECORD_FORMAT_VERSION, SYSTEM_NAME};
    use uuid::Uuid;

    fn device(n: u128) -> DeviceId {
        DeviceId(Uuid::from_u128(n))
    }

    fn record(version: u8, revision: u64, devices: [u128; 2]) -> MetadataRecord {
        let key = "k";
        MetadataRecord {
            format_version: RECORD_FORMAT_VERSION,
            system: SYSTEM_NAME.to_string(),
            bucket: "default".to_string(),
            key: key.to_string(),
            key_hash: hash_key(key.as_bytes()),
            version: VersionId([version; 16]),
            created: OffsetDateTime::UNIX_EPOCH,
            size: 1,
            object_checksum: BlockChecksum(0),
            k: 1,
            m: 1,
            block_size: 65536,
            shards: vec![
                ShardLocation {
                    index: 0,
                    device: device(devices[0]),
                },
                ShardLocation {
                    index: 1,
                    device: device(devices[1]),
                },
            ],
            revision,
            content_type: None,
            user_metadata: BTreeMap::new(),
        }
    }

    fn node(n: u128) -> NodeId {
        NodeId(Uuid::from_u128(n))
    }

    fn streamed(
        device_number: u128,
        record: &MetadataRecord,
        shard_present: bool,
    ) -> StreamedRecord {
        StreamedRecord {
            device: device(device_number),
            record: record.clone(),
            shard_present,
        }
    }

    /// SPEC 5.6: an unavailable device is left out of placement like a
    /// full one; the write goes ahead if k+m remain and is refused,
    /// naming the unavailable device, if not.
    #[test]
    fn placement_leaves_out_an_unavailable_device() {
        let status = |n: u128, available: bool| DeviceStatus {
            device: device(n),
            node: node(1),
            state: DeviceState::Active,
            available,
            label: None,
            node_label: None,
            total_bytes: 1 << 40,
            free_bytes: 1 << 40,
        };
        let scheme = Scheme::new(2, 1).expect("scheme");
        let all_present = [
            status(1, true),
            status(2, true),
            status(3, true),
            status(4, true),
        ];
        match place(&all_present, scheme, 1000) {
            Ok(chosen) => assert_eq!(chosen.len(), 3),
            Err(_) => panic!("a full set of available devices was refused"),
        }
        let one_gone_of_four = [
            status(1, true),
            status(2, true),
            status(3, true),
            status(4, false),
        ];
        match place(&one_gone_of_four, scheme, 1000) {
            Ok(chosen) => assert!(chosen.iter().all(|d| d.device != device(4))),
            Err(_) => panic!("three available devices are enough for 2+1"),
        }
        let one_gone_of_three = [status(1, true), status(2, true), status(3, false)];
        match place(&one_gone_of_three, scheme, 1000) {
            Err(Failure::Error(detail)) => {
                assert_eq!(detail.code, ErrorCode::InsufficientDevices);
                assert!(
                    detail.message.contains("1 active device(s) unavailable"),
                    "{}",
                    detail.message
                );
                assert!(
                    detail.message.contains(&device(3).0.to_string()),
                    "{}",
                    detail.message
                );
            }
            Err(_) => panic!("refused for another reason"),
            Ok(_) => panic!("placed on an unavailable device"),
        }
    }

    fn fixed(owner: NodeId, items: Vec<Result<StreamedRecord, ErrorDetail>>) -> RecordSource {
        fixed_on(owner, DeviceId(Uuid::nil()), items)
    }

    /// A source that stands for `device`'s stream, so that a copy the
    /// device should have had and does not is a copy gone missing, not a
    /// device gone from the document.
    fn fixed_on(
        owner: NodeId,
        device: DeviceId,
        items: Vec<Result<StreamedRecord, ErrorDetail>>,
    ) -> RecordSource {
        RecordSource::Fixed {
            owner,
            device,
            items: items.into(),
        }
    }

    /// SPEC 5.6: a stream that fails as unavailable is skipped, and the
    /// copies its device would hold are not expected, so the versions
    /// naming that device are not reported inconsistent; the node's own
    /// scrub reported the device once.
    #[tokio::test]
    async fn the_merge_skips_an_unavailable_device_without_reporting_its_versions() {
        let shared = record(1, 0, [1, 2]);
        let a = fixed(node(1), vec![Ok(streamed(1, &shared, true))]);
        let b = RecordSource::Fixed {
            owner: node(2),
            device: device(2),
            items: vec![Err(ErrorDetail::new(
                ErrorCode::DeviceUnavailable,
                "directory gone",
            ))]
            .into(),
        };
        let (outcome, events) = merged(vec![a, b]).await;
        assert_eq!(outcome.versions_checked, 1, "{events:?}");
        assert_eq!(outcome.stopped_after, None, "{events:?}");
        assert_eq!(outcome.findings, 0, "{events:?}");
        // Not damage, but exposure, reported once at the end: the one
        // version has its shard on device 2, which is m = 1 out, so it is
        // readable and one further loss away from not being; the count
        // by shards available says the same: 1 of 2, at k = 1.
        assert_eq!(
            events,
            vec![
                ScrubEvent::CrossCheckAvailability {
                    versions_checked: 1,
                    versions: vec![ShardAvailability {
                        shards_total: 2,
                        shards_available: 1,
                        k: 1,
                        versions: 1,
                    }],
                },
                ScrubEvent::CrossCheckExposure {
                    unread: vec![DeviceExposure {
                        device: device(2),
                        versions: 1,
                    }],
                    versions_checked: 1,
                    versions_with_shards_out: 1,
                    versions_at_the_limit: 1,
                    versions_unreadable: 0,
                },
            ]
        );
    }

    /// The exposure counts by how many of a version's shards are on
    /// unread devices against its m: none, within m, at m, beyond m.
    #[test]
    fn exposure_classifies_a_version_by_shards_out_against_m() {
        let mut exposure = Exposure::default();
        let unread: BTreeSet<DeviceId> = [device(2), device(3)].into_iter().collect();
        // k = 1, m = 1 on devices 1 and 2: one out is at the limit.
        exposure.note(&record(1, 0, [1, 2]), &unread);
        // Devices 1 and 4: nothing out.
        exposure.note(&record(2, 0, [1, 4]), &unread);
        // Devices 2 and 3: two out of m = 1, unreadable.
        exposure.note(&record(3, 0, [2, 3]), &unread);
        assert_eq!(exposure.with_shards_out, 2);
        assert_eq!(exposure.at_the_limit, 1);
        assert_eq!(exposure.unreadable, 1);
        assert_eq!(exposure.by_device.get(&device(2)), Some(&2));
        assert_eq!(exposure.by_device.get(&device(3)), Some(&1));
        assert!(exposure.event(&BTreeSet::new(), 3).is_none());
    }

    /// Run a merge over fixed sources and collect its events. The
    /// channel is bounded, as in the scrub, so the events are drained
    /// while the merge runs; draining afterwards would block a merge that
    /// sends more events than the buffer holds.
    async fn merged(sources: Vec<RecordSource>) -> (CrossCheckOutcome, Vec<ScrubEvent>) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
        let merge =
            tokio::spawn(async move { merge_sources(sources, sender, &BTreeSet::new()).await });
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        (merge.await.expect("merge task"), events)
    }

    /// SPEC 20.1.2.2: each version is judged from the group of its copies
    /// across the streams; a stream that fails ends the merge after the
    /// group in hand, with the count of versions checked.
    #[tokio::test]
    async fn the_merge_checks_each_version_from_its_copies_and_stops_where_a_stream_fails() {
        // Three versions in hash order, whatever their key text.
        let mut records: Vec<MetadataRecord> = (1..=3u8)
            .map(|v| {
                let mut r = record(v, 0, [1, 2]);
                r.key = format!("key-{v}");
                r.key_hash = hash_key(r.key.as_bytes());
                r
            })
            .collect();
        records.sort_by_key(|r| (r.key_hash, r.version));
        let (intact, missing, unchecked) = (&records[0], &records[1], &records[2]);
        let a = fixed(
            node(1),
            vec![
                Ok(streamed(1, intact, true)),
                Ok(streamed(1, missing, true)),
                Ok(streamed(1, unchecked, true)),
            ],
        );
        // Device 2 lacks the second version's shard, then its stream dies.
        let b = fixed(
            node(2),
            vec![
                Ok(streamed(2, intact, true)),
                Ok(streamed(2, missing, false)),
                Err(ErrorDetail::new(ErrorCode::NodeUnreachable, "gone")),
            ],
        );
        let (outcome, events) = merged(vec![a, b]).await;
        assert_eq!(outcome.versions_checked, 2, "{events:?}");
        assert_eq!(outcome.stopped_after, Some(2));
        assert_eq!(outcome.findings, 1);
        assert_eq!(
            outcome.damaged_keys.iter().collect::<Vec<_>>(),
            vec![&missing.key]
        );
        assert!(matches!(
            &events[0],
            ScrubEvent::ClusterFinding(ClusterFinding::ShardMissingOnDevice { key, device: d, shard_index: 1, .. })
                if *key == missing.key && *d == device(2)
        ));
        assert!(matches!(
            &events[1],
            ScrubEvent::CrossCheckStopped { node: n, versions_checked: 2, versions_unchecked: None, .. }
                if *n == node(2)
        ));
        assert!(
            matches!(
                &events[2],
                ScrubEvent::CrossCheckAvailability {
                    versions_checked: 2,
                    ..
                }
            ),
            "{events:?}"
        );
        assert_eq!(events.len(), 3, "{events:?}");
    }

    /// A copy missing on a device that has a stream is fewer than the
    /// record lists; a shard placed on a device with no stream, removed
    /// or gone from the document, is lost (18.3), one finding per shard;
    /// a run over clean copies reports progress and nothing else.
    #[tokio::test]
    async fn the_merge_reports_inconsistent_copies_lost_shards_and_progress() {
        let only_on_one = record(1, 0, [1, 2]);
        let mut on_gone_device = record(2, 0, [1, 3]);
        on_gone_device.key = "gone".to_string();
        on_gone_device.key_hash = hash_key(b"gone");
        let a = fixed_on(
            node(1),
            device(1),
            vec![
                Ok(streamed(1, &only_on_one, true)),
                Ok(streamed(1, &on_gone_device, true)),
            ],
        );
        let b = fixed_on(node(2), device(2), vec![]);
        let (outcome, events) = merged(vec![a, b]).await;
        assert_eq!(outcome.versions_checked, 2);
        assert_eq!(outcome.stopped_after, None);
        assert_eq!(outcome.findings, 2, "{events:?}");
        assert!(
            events.iter().any(|e| matches!(e,
            ScrubEvent::ClusterFinding(ClusterFinding::RecordsInconsistent { key, detail, .. })
                if key == "k" && detail.contains("1 record copies found, 2 expected"))),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e,
            ScrubEvent::ClusterFinding(ClusterFinding::ShardLost { key, device: d, shard_index: 1, .. })
                if key == "gone" && *d == device(3))),
            "{events:?}"
        );
        // The inconsistent version's shards are not known and not counted;
        // the lost shard leaves the other version with 1 of 2.
        assert!(
            events.iter().any(|e| matches!(e,
            ScrubEvent::CrossCheckAvailability { versions_checked: 2, versions }
                if *versions == vec![ShardAvailability { shards_total: 2, shards_available: 1, k: 1, versions: 1 }])),
            "{events:?}"
        );

        // Progress: two clean streams of PROGRESS_EVERY_VERSIONS + 1 versions.
        let count = PROGRESS_EVERY_VERSIONS + 1;
        let make = |n: u128| {
            fixed(
                node(n),
                (0..count)
                    .map(|i| {
                        let mut r = record(0, 0, [1, 2]);
                        r.version = VersionId((i as u128).to_be_bytes());
                        Ok(streamed(n, &r, true))
                    })
                    .collect(),
            )
        };
        let (outcome, events) = merged(vec![make(1), make(2)]).await;
        assert_eq!(outcome.versions_checked, count);
        assert_eq!(outcome.findings, 0);
        assert!(
            matches!(
                events.as_slice(),
                [
                    ScrubEvent::CrossCheckProgress { versions_checked, .. },
                    ScrubEvent::CrossCheckAvailability { versions, .. },
                ] if *versions_checked == PROGRESS_EVERY_VERSIONS
                    && *versions == vec![ShardAvailability { shards_total: 2, shards_available: 2, k: 1, versions: count }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_drain_moves_the_shard_the_current_record_places_here() {
        let copy = record(1, 0, [1, 2]);
        match shard_to_drain(device(2), &copy, std::slice::from_ref(&copy)) {
            Ok(index) => assert_eq!(index, ShardIndex(1)),
            Err(_) => panic!("the current record places shard 1 here"),
        }
    }

    #[test]
    fn a_copy_the_current_record_no_longer_places_here_is_stale() {
        // The device's copy is revision 0 placing shard 1 here; the
        // current record (revision 1) moved shard 1 to device 3.
        let copy = record(1, 0, [1, 2]);
        let current = record(1, 1, [1, 3]);
        match shard_to_drain(device(2), &copy, &[current]) {
            Err(Failure::Error(detail)) => {
                assert_eq!(detail.code, ErrorCode::RecordsInconsistent);
                assert!(detail.message.contains("stale copy"), "{}", detail.message);
                assert_eq!(detail.device, Some(device(2)));
            }
            Err(Failure::Close(_)) => panic!("expected an error, not a close"),
            Ok(index) => panic!("expected stale, got shard {index}"),
        }
    }

    #[test]
    fn a_version_absent_from_the_current_versions_is_stale_too() {
        // Copies of the version exist (the caller checked) but none is
        // current: for example a replaced version whose deletion did not
        // reach this device.
        let copy = record(1, 0, [1, 2]);
        let newer = record(2, 0, [1, 2]);
        assert!(matches!(
            shard_to_drain(device(2), &copy, &[newer]),
            Err(Failure::Error(ref d)) if d.message.contains("stale copy")
        ));
    }
}
