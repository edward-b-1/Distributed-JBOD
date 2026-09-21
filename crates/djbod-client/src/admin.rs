//! Administration: changing the cluster document without a master (SPEC
//! 6.2.6) and every procedure built on it, resolving stragglers (6.2.6.2),
//! and the forced removal of a dead node (6.2.6.3).
//!
//! Every function here talks to nodes as a *client* over the native
//! protocol, so it runs the same from an administrator's machine, from
//! the web UI, or from a node. The procedures that touch a node's own
//! state directory, joining, adding devices, and startup adoption, are in
//! the node crate, built on these.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use djbod_core::cluster::{ClusterDocument, ClusterDocumentError, DeviceState, NodeId};
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::version::VersionId;
use djbod_proto::handshake::Hello;
use djbod_proto::message::{ErrorCode, ErrorDetail, RecordCursor, Request, Response};

use crate::connection::{ClientError, Connection};
use crate::transport::Connector;

/// What one node answered when asked for its document.
#[derive(Debug)]
pub struct NodeDocument {
    pub node: NodeId,
    /// The node's label in the document that was asked, if any.
    pub label: Option<String>,
    pub address: String,
    /// The node's software build from its `Hello`; `None` when it was
    /// unreachable or built before builds were sent (19.1.5).
    pub build: Option<String>,
    pub result: Result<ClusterDocument, String>,
}

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("{node} at {address} has no usable address: {reason}")]
    BadAddress {
        node: NodeId,
        address: String,
        reason: String,
    },
    #[error("cannot reach {node} at {address}: {reason}")]
    Unreachable {
        node: NodeId,
        address: String,
        reason: String,
    },
    #[error("nodes hold different document versions: {0:?}; run `djbod cluster sync` first")]
    VersionsDiffer(Vec<(NodeId, u64)>),
    #[error(
        "{a} and {b} both hold document version {version} but the documents differ; this should be impossible and needs an administrator: compare the two cluster.json files"
    )]
    Diverged { version: u64, a: NodeId, b: NodeId },
    #[error("nodes hold version {found}, not the version {expected} the proposal was built from")]
    StaleProposal { expected: u64, found: u64 },
    #[error(
        "the first-listed node {node} refused: {reason}; another change won, refetch and retry"
    )]
    Superseded { node: NodeId, reason: String },
    #[error(
        "applied to {applied:?} but {failed} refused or could not be reached ({reason}); the cluster has stragglers, run `djbod cluster sync`"
    )]
    Partial {
        applied: Vec<NodeId>,
        failed: NodeId,
        reason: String,
    },
    #[error("peer {address} belongs to cluster {found}, not {expected}")]
    WrongCluster {
        address: SocketAddr,
        expected: Uuid,
        found: Uuid,
    },
    #[error("peer {address} answered {response} instead of a document")]
    UnexpectedResponse {
        address: SocketAddr,
        response: String,
    },
    #[error("cannot reach peer {address}: {reason}")]
    PeerUnreachable { address: SocketAddr, reason: String },
    #[error("gave up after {0} attempts; another change kept winning")]
    TooManyRetries(u32),
    /// A proposed document the validator refuses: a bad label or name, a
    /// scheme the devices cannot carry, an address problem, and the like.
    #[error("the proposed document is not valid: {0}")]
    Document(#[from] ClusterDocumentError),
    #[error("{0} is not in the cluster document")]
    UnknownDevice(DeviceId),
    #[error("no device is named {0:?}, as a UUID or a label")]
    UnknownDeviceName(String),
    #[error("no node is named {0:?}, as a UUID or a label")]
    UnknownNodeName(String),
    #[error("{0} is not in the cluster document")]
    UnknownNode(NodeId),
    #[error(
        "{what} is still named by the current record of {versions} version(s), for example {examples:?}; drain it first (`djbod cluster set-state`, `djbod cluster drain`)"
    )]
    StillReferenced {
        what: String,
        versions: usize,
        examples: Vec<String>,
    },
    #[error(
        "{0} is active; a device is removed only once it is draining and drained (`djbod cluster set-state`, `djbod cluster drain`), so that no write can land on it between the reference scan and the removal"
    )]
    DeviceActive(DeviceId),
    #[error(
        "{node} still has active device(s) {devices:?}; set each draining and drain it first (`djbod cluster set-state`, `djbod cluster drain`)"
    )]
    NodeHasActiveDevices {
        node: NodeId,
        devices: Vec<DeviceId>,
    },
    #[error("{node} at {address} answered; a live node is removed with set-state, drain, and remove-node, not --force")]
    NodeIsAlive { node: NodeId, address: String },
    #[error("cannot remove the only node of the cluster")]
    LastNode,
    #[error(
        "{node} at {address} has no TLS material loaded; give it [tls] paths and restart it before moving the transport off plain (SPEC 19.1.6.4)"
    )]
    NodeNotTlsReady { node: NodeId, address: String },
    #[error(
        "the cluster's transport is {transport} but no TLS material was given; pass --tls-cert, --tls-key, --tls-ca or a [tls] table (SPEC 19.1.6.2)"
    )]
    TlsRequired {
        transport: djbod_core::cluster::Transport,
    },
    #[error(
        "the cluster has {active} active device(s) but scheme {k}+{m} needs {needed}; add devices first"
    )]
    TooFewActiveDevices {
        active: usize,
        needed: usize,
        k: u8,
        m: u8,
    },
}

pub fn first_address(document: &ClusterDocument, node: NodeId) -> Result<SocketAddr, AdminError> {
    let entry = document.node(node).expect("node taken from this document");
    let text = entry.addresses.first().cloned().unwrap_or_default();
    text.parse()
        .map_err(|e: std::net::AddrParseError| AdminError::BadAddress {
            node,
            address: text.clone(),
            reason: e.to_string(),
        })
}

/// Fetch one node's document, connecting as a client.
pub async fn fetch_document(
    connector: &Connector,
    address: SocketAddr,
    cluster_id: Uuid,
) -> Result<ClusterDocument, AdminError> {
    fetch_document_and_hello(connector, address, cluster_id)
        .await
        .map(|(document, _)| document)
}

/// `fetch_document`, also returning the node's `Hello`, which names its
/// build.
async fn fetch_document_and_hello(
    connector: &Connector,
    address: SocketAddr,
    cluster_id: Uuid,
) -> Result<(ClusterDocument, Hello), AdminError> {
    let mut connection =
        Connection::connect_with(connector, address, Connection::client_hello(cluster_id))
            .await
            .map_err(|e| match e {
                ClientError::Hello(djbod_proto::handshake::HelloError::ClusterId {
                    peer,
                    ours,
                }) => AdminError::WrongCluster {
                    address,
                    expected: ours,
                    found: peer,
                },
                ClientError::Remote(ErrorDetail {
                    code: ErrorCode::ProtocolViolation,
                    message,
                    ..
                }) if message.contains("cluster") => AdminError::PeerUnreachable {
                    address,
                    reason: message,
                },
                other => AdminError::PeerUnreachable {
                    address,
                    reason: other.to_string(),
                },
            })?;
    match connection.request(Request::GetClusterConfig).await {
        Ok(Response::GetClusterConfig { document }) => {
            Ok((document, connection.peer_hello().clone()))
        }
        Ok(other) => Err(AdminError::UnexpectedResponse {
            address,
            response: format!("{other:?}"),
        }),
        Err(e) => Err(AdminError::PeerUnreachable {
            address,
            reason: e.to_string(),
        }),
    }
}

/// Ask every node listed in `document` for its own copy (6.2.6 step 1;
/// also `djbod cluster show`).
pub async fn fetch_all(connector: &Connector, document: &ClusterDocument) -> Vec<NodeDocument> {
    fetch_all_except(connector, document, None).await
}

pub async fn fetch_all_except(
    connector: &Connector,
    document: &ClusterDocument,
    skip: Option<NodeId>,
) -> Vec<NodeDocument> {
    let mut reports = Vec::with_capacity(document.nodes.len());
    for entry in document.nodes.iter().filter(|n| Some(n.id) != skip) {
        let address_text = entry.addresses.first().cloned().unwrap_or_default();
        let (build, result) = match first_address(document, entry.id) {
            Ok(address) => {
                match fetch_document_and_hello(connector, address, document.cluster_id).await {
                    Ok((theirs, hello)) => (hello.build, Ok(theirs)),
                    Err(e) => (None, Err(e.to_string())),
                }
            }
            Err(e) => (None, Err(e.to_string())),
        };
        reports.push(NodeDocument {
            node: entry.id,
            label: entry.label.clone(),
            address: address_text,
            build,
            result,
        });
    }
    reports
}

/// Propose `next` as the successor of `current` (6.2.6): every node must
/// be reachable and hold `current.version`; then apply in document
/// order, stopping at the first refusal.
pub async fn propose(
    connector: &Connector,
    current: &ClusterDocument,
    next: &ClusterDocument,
) -> Result<(), AdminError> {
    propose_skipping(connector, current, next, None).await
}

/// `propose`, treating `skip` as having acknowledged (6.2.6.3 step 2):
/// it is neither asked for its version nor sent the new document. Also
/// how a node not yet serving proposes its own address (18.1.2.1).
pub async fn propose_skipping(
    connector: &Connector,
    current: &ClusterDocument,
    next: &ClusterDocument,
    skip: Option<NodeId>,
) -> Result<(), AdminError> {
    next.validate()?;
    // Step 1: check.
    let mut versions = Vec::new();
    let mut documents: Vec<(NodeId, ClusterDocument)> = Vec::new();
    for report in fetch_all_except(connector, current, skip).await {
        match report.result {
            Ok(document) => {
                versions.push((report.node, document.version));
                documents.push((report.node, document));
            }
            Err(reason) => {
                return Err(AdminError::Unreachable {
                    node: report.node,
                    address: report.address,
                    reason,
                })
            }
        }
    }
    if versions.iter().any(|(_, v)| *v != current.version) {
        if versions.iter().all(|(_, v)| *v == versions[0].1) {
            return Err(AdminError::StaleProposal {
                expected: current.version,
                found: versions[0].1,
            });
        }
        return Err(AdminError::VersionsDiffer(versions));
    }
    // Same version everywhere must mean the same document everywhere
    // (6.2.6.1). Anything else is a bug or a hand-edited file, and no
    // automatic step is safe.
    check_same_content(&documents)?;
    // Step 2: apply in document order.
    let mut applied = Vec::new();
    for (position, entry) in current
        .nodes
        .iter()
        .filter(|n| Some(n.id) != skip)
        .enumerate()
    {
        let address = first_address(current, entry.id)?;
        let outcome = apply_to(connector, address, current.cluster_id, next).await;
        match outcome {
            Ok(()) => applied.push(entry.id),
            Err(reason) if position == 0 => {
                return Err(AdminError::Superseded {
                    node: entry.id,
                    reason,
                })
            }
            Err(reason) => {
                return Err(AdminError::Partial {
                    applied,
                    failed: entry.id,
                    reason,
                })
            }
        }
    }
    Ok(())
}

/// Two nodes holding the same version must hold the same document.
fn check_same_content(documents: &[(NodeId, ClusterDocument)]) -> Result<(), AdminError> {
    for (i, (node_a, doc_a)) in documents.iter().enumerate() {
        for (node_b, doc_b) in &documents[..i] {
            if doc_a.version == doc_b.version && doc_a != doc_b {
                return Err(AdminError::Diverged {
                    version: doc_a.version,
                    a: *node_b,
                    b: *node_a,
                });
            }
        }
    }
    Ok(())
}

async fn apply_to(
    connector: &Connector,
    address: SocketAddr,
    cluster_id: Uuid,
    document: &ClusterDocument,
) -> Result<(), String> {
    let mut connection =
        Connection::connect_with(connector, address, Connection::client_hello(cluster_id))
            .await
            .map_err(|e| e.to_string())?;
    match connection
        .request(Request::ApplyClusterConfig {
            document: document.clone(),
        })
        .await
    {
        Ok(Response::ApplyClusterConfig) => Ok(()),
        Ok(other) => Err(format!("unexpected response {other:?}")),
        Err(ClientError::Remote(detail)) => Err(detail.message),
        Err(e) => Err(e.to_string()),
    }
}

/// Result of a sync (6.2.6.2).
#[derive(Debug, Default)]
pub struct SyncReport {
    pub highest_version: u64,
    pub updated: Vec<NodeId>,
    pub already_current: Vec<NodeId>,
    pub unreachable: Vec<(NodeId, String)>,
}

/// Bring every reachable node up to the highest document version any of
/// them holds. Safe because at most one document exists per version
/// (6.2.6.1). `seed` is any node's address.
pub async fn sync(
    connector: &Connector,
    seed: SocketAddr,
    cluster_id: Uuid,
) -> Result<SyncReport, AdminError> {
    let seed_document = fetch_document(connector, seed, cluster_id).await?;
    // The seed's membership list may itself be stale; use the highest
    // version's list, found by asking everyone the seed knows about.
    let mut highest = seed_document.clone();
    let mut reports = fetch_all(connector, &seed_document).await;
    for report in &reports {
        if let Ok(document) = &report.result {
            if document.version > highest.version {
                highest = document.clone();
            }
        }
    }
    if highest.version != seed_document.version {
        // Re-ask with the fuller membership list.
        reports = fetch_all(connector, &highest).await;
    }
    let mut result = SyncReport {
        highest_version: highest.version,
        ..SyncReport::default()
    };
    let held: Vec<(NodeId, ClusterDocument)> = reports
        .iter()
        .filter_map(|r| r.result.as_ref().ok().map(|d| (r.node, d.clone())))
        .collect();
    check_same_content(&held)?;
    for report in reports {
        match report.result {
            Ok(document) if document.version == highest.version => {
                result.already_current.push(report.node)
            }
            Ok(_) => {
                let address = first_address(&highest, report.node)?;
                match apply_to(connector, address, cluster_id, &highest).await {
                    Ok(()) => result.updated.push(report.node),
                    Err(reason) => result.unreachable.push((report.node, reason)),
                }
            }
            Err(reason) => result.unreachable.push((report.node, reason)),
        }
    }
    Ok(result)
}

/// How many times `join` and `add_devices` retry after being superseded.
pub const MAX_PROPOSAL_ATTEMPTS: u32 = 5;

/// Change one device's state in the document (18.2.1) and nothing else.
/// Returns the document that now holds the state and whether a new
/// version was proposed; asking for the state a device already has is a
/// no-op, so the command is safe to repeat.
pub async fn set_device_state(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    device: DeviceId,
    state: DeviceState,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let Some(entry) = current.device(device) else {
            return Err(AdminError::UnknownDevice(device));
        };
        if entry.state == state {
            return Ok((current, false));
        }
        let mut next = current.clone();
        next.version += 1;
        for candidate in next.devices.iter_mut() {
            if candidate.id == device {
                candidate.state = state;
            }
        }
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Change the global scheme, and optionally the block size, in the
/// document (18.9): from then on new writes use the new values. Existing
/// versions keep their own (6.3) until re-encoded. Refused when fewer
/// active devices exist than the new k+m, since every write would fail
/// (7.3). Returns the document and whether anything changed.
pub async fn set_scheme(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    k: u8,
    m: u8,
    block_size: Option<u64>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let block_size = block_size.unwrap_or(current.block_size);
        if current.k == k && current.m == m && current.block_size == block_size {
            return Ok((current, false));
        }
        let active = current
            .devices
            .iter()
            .filter(|d| d.state == DeviceState::Active)
            .count();
        let needed = k as usize + m as usize;
        if active < needed {
            return Err(AdminError::TooFewActiveDevices {
                active,
                needed,
                k,
                m,
            });
        }
        let mut next = current.clone();
        next.version += 1;
        next.k = k;
        next.m = m;
        next.block_size = block_size;
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Change the key length, object size, and user metadata limits in the
/// document (9.1.5, 9.3.1, 9.4.2). Any may be left as it is. Returns the
/// document and whether anything changed.
pub async fn set_limits(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    max_key_bytes: Option<u64>,
    max_object_bytes: Option<u64>,
    max_user_metadata_bytes: Option<u64>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let mut next = current.clone();
        next.max_key_bytes = max_key_bytes.unwrap_or(current.max_key_bytes);
        next.max_object_bytes = max_object_bytes.unwrap_or(current.max_object_bytes);
        next.max_user_metadata_bytes =
            max_user_metadata_bytes.unwrap_or(current.max_user_metadata_bytes);
        if next == current {
            return Ok((current, false));
        }
        next.version += 1;
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// The device a UUID or label names, in the current document.
pub async fn resolve_device(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    name: &str,
) -> Result<DeviceId, AdminError> {
    let document = fetch_document(connector, peer, cluster_id).await?;
    document
        .device_by_name(name)
        .map(|d| d.id)
        .ok_or_else(|| AdminError::UnknownDeviceName(name.to_string()))
}

/// The node a UUID or label names, in the current document.
pub async fn resolve_node(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    name: &str,
) -> Result<NodeId, AdminError> {
    let document = fetch_document(connector, peer, cluster_id).await?;
    document
        .node_by_name(name)
        .map(|n| n.id)
        .ok_or_else(|| AdminError::UnknownNodeName(name.to_string()))
}

/// Set or clear a node's label (SPEC 6.2.5.1). Returns the document and
/// whether anything changed.
pub async fn set_node_label(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    node: NodeId,
    label: Option<String>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let Some(entry) = current.node(node) else {
            return Err(AdminError::UnknownNode(node));
        };
        if entry.label == label {
            return Ok((current, false));
        }
        let mut next = current.clone();
        next.version += 1;
        for candidate in next.nodes.iter_mut() {
            if candidate.id == node {
                candidate.label = label.clone();
            }
        }
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Set or clear the cluster's name (SPEC 6.2.5.3). Returns the document
/// and whether anything changed; the validator refuses an unusable name.
pub async fn set_cluster_name(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    name: Option<String>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        if current.name == name {
            return Ok((current, false));
        }
        let mut next = current.clone();
        next.version += 1;
        next.name = name.clone();
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Replace a node's address list (SPEC 6.2.5.2). Returns the document and
/// whether anything changed; the validator refuses an empty list, an
/// address that is not `ip:port`, or one already listed for a node. The
/// node is reached at its currently listed address for the proposal, as
/// every node is for every change.
pub async fn set_node_addresses(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    node: NodeId,
    addresses: Vec<String>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let Some(entry) = current.node(node) else {
            return Err(AdminError::UnknownNode(node));
        };
        if entry.addresses == addresses {
            return Ok((current, false));
        }
        let next = with_node_addresses(&current, node, addresses.clone());
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// The successor of `current` in which `node` is listed at `addresses`.
pub fn with_node_addresses(
    current: &ClusterDocument,
    node: NodeId,
    addresses: Vec<String>,
) -> ClusterDocument {
    let mut next = current.clone();
    next.version += 1;
    for candidate in next.nodes.iter_mut() {
        if candidate.id == node {
            candidate.addresses = addresses.clone();
        }
    }
    next
}

/// Set or clear a device's label (SPEC 6.2.5.1). Returns the document and
/// whether anything changed; the validator refuses a duplicate or an
/// unusable label.
pub async fn set_device_label(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    device: DeviceId,
    label: Option<String>,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let Some(entry) = current.device(device) else {
            return Err(AdminError::UnknownDevice(device));
        };
        if entry.label == label {
            return Ok((current, false));
        }
        let mut next = current.clone();
        next.version += 1;
        for candidate in next.devices.iter_mut() {
            if candidate.id == device {
                candidate.label = label.clone();
            }
        }
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Change the cluster's transport (SPEC 19.1.6.4). Moving off `plain` is
/// refused until every node reports TLS material loaded, so the change
/// cannot leave a node unable to reach its peers. Returns the document
/// and whether anything changed.
pub async fn set_transport(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    transport: djbod_core::cluster::Transport,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        if current.transport == transport {
            return Ok((current, false));
        }
        if transport != djbod_core::cluster::Transport::Plain {
            for entry in &current.nodes {
                let address = first_address(&current, entry.id)?;
                let unreachable = |reason: String| AdminError::Unreachable {
                    node: entry.id,
                    address: address.to_string(),
                    reason,
                };
                let mut connection = Connection::connect_with(
                    connector,
                    address,
                    Connection::client_hello(cluster_id),
                )
                .await
                .map_err(|e| unreachable(e.to_string()))?;
                match connection.request(Request::LocalStatus).await {
                    Ok(Response::LocalStatus {
                        tls_ready: true, ..
                    }) => {}
                    Ok(Response::LocalStatus { .. }) => {
                        return Err(AdminError::NodeNotTlsReady {
                            node: entry.id,
                            address: address.to_string(),
                        })
                    }
                    Ok(other) => return Err(unreachable(format!("unexpected response {other:?}"))),
                    Err(e) => return Err(unreachable(e.to_string())),
                }
            }
        }
        let mut next = current.clone();
        next.version += 1;
        next.transport = transport;
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

// ------------------------------------------------------------- REMOVAL

/// A version whose current record places shards on the devices being
/// looked for (18.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReference {
    pub key: String,
    pub version: VersionId,
    /// How many of the version's shards are on those devices.
    pub shards: usize,
    /// The version's parity count: more than `m` shards there cannot be
    /// rebuilt.
    pub m: u8,
}

impl VersionReference {
    pub fn recoverable(&self) -> bool {
        self.shards <= self.m as usize
    }
}

/// Find every version whose current record names any of `devices`
/// (18.5): ask each node except `skip` for the records on each of its
/// devices, keep the highest revision per version, and count. Runs from
/// the client, connecting to every node.
pub async fn scan_references(
    connector: &Connector,
    document: &ClusterDocument,
    devices: &[DeviceId],
    skip: Option<NodeId>,
) -> Result<Vec<VersionReference>, AdminError> {
    let mut current: BTreeMap<(String, VersionId), MetadataRecord> = BTreeMap::new();
    for entry in document.nodes.iter().filter(|n| Some(n.id) != skip) {
        let address = first_address(document, entry.id)?;
        let unreachable = |reason: String| AdminError::Unreachable {
            node: entry.id,
            address: address.to_string(),
            reason,
        };
        let mut connection = Connection::connect_with(
            connector,
            address,
            Connection::client_hello(document.cluster_id),
        )
        .await
        .map_err(|e| unreachable(e.to_string()))?;
        for device in document
            .devices
            .iter()
            .filter(|d| d.node == entry.id && d.state != DeviceState::Removed)
        {
            let mut records = Vec::new();
            let mut after: Option<RecordCursor> = None;
            loop {
                match connection
                    .request(Request::LocalRecords {
                        device: device.id,
                        after: after.clone(),
                    })
                    .await
                {
                    Ok(Response::LocalRecords {
                        records: page,
                        truncated,
                    }) => {
                        after = page.last().map(|r| RecordCursor {
                            key: r.key.clone(),
                            version: r.version,
                        });
                        records.extend(page);
                        if !truncated || after.is_none() {
                            break;
                        }
                    }
                    Ok(other) => return Err(unreachable(format!("unexpected response {other:?}"))),
                    Err(e) => return Err(unreachable(e.to_string())),
                }
            }
            for record in records {
                let slot = current.entry((record.key.clone(), record.version));
                match slot {
                    std::collections::btree_map::Entry::Vacant(v) => {
                        v.insert(record);
                    }
                    std::collections::btree_map::Entry::Occupied(mut o) => {
                        if record.revision > o.get().revision {
                            o.insert(record);
                        }
                    }
                }
            }
        }
    }
    let mut references = Vec::new();
    for ((key, version), record) in current {
        let shards = record
            .shards
            .iter()
            .filter(|s| devices.contains(&s.device))
            .count();
        if shards > 0 {
            references.push(VersionReference {
                key,
                version,
                shards,
                m: record.m,
            });
        }
    }
    Ok(references)
}

fn still_referenced(what: String, references: &[VersionReference]) -> AdminError {
    AdminError::StillReferenced {
        what,
        versions: references.len(),
        examples: references.iter().take(5).map(|r| r.key.clone()).collect(),
    }
}

/// Mark a device `removed` (18.2.1) once no current record names it.
/// Returns the document and whether anything changed.
pub async fn remove_device(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    device: DeviceId,
) -> Result<(ClusterDocument, bool), AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        let Some(entry) = current.device(device) else {
            return Err(AdminError::UnknownDevice(device));
        };
        if entry.state == DeviceState::Removed {
            return Ok((current, false));
        }
        // Only a draining device can be removed: no write can place a
        // shard on it, so the scan below cannot be invalidated between the
        // scan and the proposal.
        if entry.state == DeviceState::Active {
            return Err(AdminError::DeviceActive(device));
        }
        let references = scan_references(connector, &current, &[device], None).await?;
        if !references.is_empty() {
            return Err(still_referenced(format!("{device}"), &references));
        }
        let mut next = current.clone();
        next.version += 1;
        for candidate in next.devices.iter_mut() {
            if candidate.id == device {
                candidate.state = DeviceState::Removed;
            }
        }
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok((next, true)),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Drop a live node and its devices from the document (18.2.1) once no
/// current record names any of its devices. The node acknowledges the
/// document like every other and then stops serving.
pub async fn remove_node(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    node: NodeId,
) -> Result<ClusterDocument, AdminError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let current = fetch_document(connector, peer, cluster_id).await?;
        if current.node(node).is_none() {
            return Err(AdminError::UnknownNode(node));
        }
        if current.nodes.len() == 1 {
            return Err(AdminError::LastNode);
        }
        let devices: Vec<DeviceId> = current
            .devices
            .iter()
            .filter(|d| d.node == node)
            .map(|d| d.id)
            .collect();
        // As for a device: every device of the node must be draining or
        // removed before the scan means anything.
        let active: Vec<DeviceId> = current
            .devices
            .iter()
            .filter(|d| d.node == node && d.state == DeviceState::Active)
            .map(|d| d.id)
            .collect();
        if !active.is_empty() {
            return Err(AdminError::NodeHasActiveDevices {
                node,
                devices: active,
            });
        }
        let references = scan_references(connector, &current, &devices, None).await?;
        if !references.is_empty() {
            return Err(still_referenced(format!("{node}"), &references));
        }
        let next = document_without_node(&current, node);
        match propose(connector, &current, &next).await {
            Ok(()) => return Ok(next),
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

fn document_without_node(current: &ClusterDocument, node: NodeId) -> ClusterDocument {
    let mut next = current.clone();
    next.version += 1;
    next.nodes.retain(|n| n.id != node);
    next.devices.retain(|d| d.node != node);
    next
}

/// What forcing the removal of a dead node will cost (6.2.6.3 step 1),
/// to be shown and confirmed before `execute_forced_removal`.
#[derive(Debug, Clone)]
pub struct ForcedRemovalPlan {
    pub current: ClusterDocument,
    pub node: NodeId,
    pub address: String,
    /// Why the node counted as dead.
    pub unreachable_because: String,
    pub devices: Vec<DeviceId>,
    /// Every version with a shard on the node's devices.
    pub affected: Vec<VersionReference>,
}

impl ForcedRemovalPlan {
    /// Versions with more than m shards on the dead node: lost for good.
    pub fn unrecoverable(&self) -> Vec<&VersionReference> {
        self.affected.iter().filter(|r| !r.recoverable()).collect()
    }
}

/// How long the forced path waits for the supposedly dead node to answer
/// before believing it is dead.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(5);

/// Step 1 of 6.2.6.3: refuse if the node answers; otherwise count what
/// its loss costs, using the other nodes' records.
pub async fn plan_forced_removal(
    connector: &Connector,
    peer: SocketAddr,
    cluster_id: Uuid,
    node: NodeId,
) -> Result<ForcedRemovalPlan, AdminError> {
    let current = fetch_document(connector, peer, cluster_id).await?;
    if current.node(node).is_none() {
        return Err(AdminError::UnknownNode(node));
    }
    if current.nodes.len() == 1 {
        return Err(AdminError::LastNode);
    }
    let address = first_address(&current, node)?;
    let unreachable_because = match tokio::time::timeout(
        LIVENESS_TIMEOUT,
        fetch_document(connector, address, cluster_id),
    )
    .await
    {
        Ok(Ok(_)) => {
            return Err(AdminError::NodeIsAlive {
                node,
                address: address.to_string(),
            })
        }
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("no answer within {} seconds", LIVENESS_TIMEOUT.as_secs()),
    };
    let devices: Vec<DeviceId> = current
        .devices
        .iter()
        .filter(|d| d.node == node)
        .map(|d| d.id)
        .collect();
    let affected = scan_references(connector, &current, &devices, Some(node)).await?;
    Ok(ForcedRemovalPlan {
        current,
        node,
        address: address.to_string(),
        unreachable_because,
        devices,
        affected,
    })
}

/// Step 2 of 6.2.6.3: propose the document without the dead node,
/// treating it as having acknowledged. Step 3, the rebuild, is one
/// `RepairObject` per affected key, run by the caller through any live
/// node.
pub async fn execute_forced_removal(
    connector: &Connector,
    plan: &ForcedRemovalPlan,
) -> Result<ClusterDocument, AdminError> {
    let next = document_without_node(&plan.current, plan.node);
    propose_skipping(connector, &plan.current, &next, Some(plan.node)).await?;
    Ok(next)
}
