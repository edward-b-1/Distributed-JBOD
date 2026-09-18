//! Membership: changing the cluster document without a master (SPEC
//! 6.2.6), joining a node (18.1.1), startup adoption (18.1.2), and
//! resolving stragglers (6.2.6.2).
//!
//! Every function here talks to nodes as a *client* over the native
//! protocol, so it can run from an administrator's machine, from a node
//! that is not yet a member, or from a node before it has opened.

use std::net::SocketAddr;
use std::path::PathBuf;

use thiserror::Error;
use uuid::Uuid;

use djbod_core::cluster::{ClusterDocument, NodeId};
use djbod_core::device::{Device, DeviceError};
use djbod_core::record::DeviceId;
use djbod_proto::message::{ErrorCode, ErrorDetail, Request, Response};

use crate::client::{ClientError, Connection};
use crate::config::NodeConfig;
use crate::node::{Node, NodeError};

/// What one node answered when asked for its document.
#[derive(Debug)]
pub struct NodeDocument {
    pub node: NodeId,
    pub address: String,
    pub result: Result<ClusterDocument, String>,
}

#[derive(Debug, Error)]
pub enum MembershipError {
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
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error("device {path} is already initialised and in the document")]
    AlreadyMember { path: PathBuf },
}

fn first_address(document: &ClusterDocument, node: NodeId) -> Result<SocketAddr, MembershipError> {
    let entry = document.node(node).expect("node taken from this document");
    let text = entry.addresses.first().cloned().unwrap_or_default();
    text.parse()
        .map_err(|e: std::net::AddrParseError| MembershipError::BadAddress {
            node,
            address: text.clone(),
            reason: e.to_string(),
        })
}

/// Fetch one node's document, connecting as a client.
pub async fn fetch_document(
    address: SocketAddr,
    cluster_id: Uuid,
) -> Result<ClusterDocument, MembershipError> {
    let mut connection = Connection::connect(address, Connection::client_hello(cluster_id))
        .await
        .map_err(|e| match e {
            ClientError::Hello(djbod_proto::handshake::HelloError::ClusterId { peer, ours }) => {
                MembershipError::WrongCluster {
                    address,
                    expected: ours,
                    found: peer,
                }
            }
            ClientError::Remote(ErrorDetail {
                code: ErrorCode::ProtocolViolation,
                message,
                ..
            }) if message.contains("cluster") => MembershipError::PeerUnreachable {
                address,
                reason: message,
            },
            other => MembershipError::PeerUnreachable {
                address,
                reason: other.to_string(),
            },
        })?;
    match connection.request(Request::GetClusterConfig).await {
        Ok(Response::GetClusterConfig { document }) => Ok(document),
        Ok(other) => Err(MembershipError::UnexpectedResponse {
            address,
            response: format!("{other:?}"),
        }),
        Err(e) => Err(MembershipError::PeerUnreachable {
            address,
            reason: e.to_string(),
        }),
    }
}

/// Ask every node listed in `document` for its own copy (6.2.6 step 1;
/// also `djbod cluster show`).
pub async fn fetch_all(document: &ClusterDocument) -> Vec<NodeDocument> {
    let mut reports = Vec::with_capacity(document.nodes.len());
    for entry in &document.nodes {
        let address_text = entry.addresses.first().cloned().unwrap_or_default();
        let result = match first_address(document, entry.id) {
            Ok(address) => fetch_document(address, document.cluster_id)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        reports.push(NodeDocument {
            node: entry.id,
            address: address_text,
            result,
        });
    }
    reports
}

/// Propose `next` as the successor of `current` (6.2.6): every node must
/// be reachable and hold `current.version`; then apply in document
/// order, stopping at the first refusal.
pub async fn propose(
    current: &ClusterDocument,
    next: &ClusterDocument,
) -> Result<(), MembershipError> {
    next.validate().map_err(NodeError::from)?;
    // Step 1: check.
    let mut versions = Vec::new();
    let mut documents: Vec<(NodeId, ClusterDocument)> = Vec::new();
    for report in fetch_all(current).await {
        match report.result {
            Ok(document) => {
                versions.push((report.node, document.version));
                documents.push((report.node, document));
            }
            Err(reason) => {
                return Err(MembershipError::Unreachable {
                    node: report.node,
                    address: report.address,
                    reason,
                })
            }
        }
    }
    if versions.iter().any(|(_, v)| *v != current.version) {
        if versions.iter().all(|(_, v)| *v == versions[0].1) {
            return Err(MembershipError::StaleProposal {
                expected: current.version,
                found: versions[0].1,
            });
        }
        return Err(MembershipError::VersionsDiffer(versions));
    }
    // Same version everywhere must mean the same document everywhere
    // (6.2.6.1). Anything else is a bug or a hand-edited file, and no
    // automatic step is safe.
    check_same_content(&documents)?;
    // Step 2: apply in document order.
    let mut applied = Vec::new();
    for (position, entry) in current.nodes.iter().enumerate() {
        let address = first_address(current, entry.id)?;
        let outcome = apply_to(address, current.cluster_id, next).await;
        match outcome {
            Ok(()) => applied.push(entry.id),
            Err(reason) if position == 0 => {
                return Err(MembershipError::Superseded {
                    node: entry.id,
                    reason,
                })
            }
            Err(reason) => {
                return Err(MembershipError::Partial {
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
fn check_same_content(documents: &[(NodeId, ClusterDocument)]) -> Result<(), MembershipError> {
    for (i, (node_a, doc_a)) in documents.iter().enumerate() {
        for (node_b, doc_b) in &documents[..i] {
            if doc_a.version == doc_b.version && doc_a != doc_b {
                return Err(MembershipError::Diverged {
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
    address: SocketAddr,
    cluster_id: Uuid,
    document: &ClusterDocument,
) -> Result<(), String> {
    let mut connection = Connection::connect(address, Connection::client_hello(cluster_id))
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
pub async fn sync(seed: SocketAddr, cluster_id: Uuid) -> Result<SyncReport, MembershipError> {
    let seed_document = fetch_document(seed, cluster_id).await?;
    // The seed's membership list may itself be stale; use the highest
    // version's list, found by asking everyone the seed knows about.
    let mut highest = seed_document.clone();
    let mut reports = fetch_all(&seed_document).await;
    for report in &reports {
        if let Ok(document) = &report.result {
            if document.version > highest.version {
                highest = document.clone();
            }
        }
    }
    if highest.version != seed_document.version {
        // Re-ask with the fuller membership list.
        reports = fetch_all(&highest).await;
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
                match apply_to(address, cluster_id, &highest).await {
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

/// Join this node to an existing cluster (18.1.1). On success the
/// configuration's state directory holds the new document and every
/// configured device is initialised and listed, so `run` may follow.
pub async fn join(
    config: &NodeConfig,
    peer: SocketAddr,
    cluster_id: Uuid,
) -> Result<ClusterDocument, MembershipError> {
    let mut current = fetch_document(peer, cluster_id).await?;
    Node::save_document_for(config, &current)?;
    let mut device_ids = Vec::with_capacity(config.devices.len());
    for path in &config.devices {
        let device = match Device::open(path, Some(cluster_id)) {
            Ok(existing) => existing, // a retry after a partial failure
            Err(DeviceError::NotInitialised { .. }) => Device::initialise(path, cluster_id)?,
            Err(e) => return Err(e.into()),
        };
        device_ids.push(device.id());
    }
    propose_with_retry(config, &mut current, &device_ids, peer, cluster_id).await
}

/// Initialise `paths` (which must be listed in the configuration) and
/// add them to the document (18.1.3). The node picks them up at its next
/// start.
pub async fn add_devices(
    config: &NodeConfig,
    paths: &[PathBuf],
    peer: SocketAddr,
    cluster_id: Uuid,
) -> Result<ClusterDocument, MembershipError> {
    let mut current = fetch_document(peer, cluster_id).await?;
    let mut device_ids: Vec<DeviceId> = Vec::new();
    for path in paths {
        let device = match Device::open(path, Some(cluster_id)) {
            Ok(existing) => {
                if current.device(existing.id()).is_some() {
                    return Err(MembershipError::AlreadyMember { path: path.clone() });
                }
                existing
            }
            Err(DeviceError::NotInitialised { .. }) => Device::initialise(path, cluster_id)?,
            Err(e) => return Err(e.into()),
        };
        device_ids.push(device.id());
    }
    propose_with_retry(config, &mut current, &device_ids, peer, cluster_id).await
}

async fn propose_with_retry(
    config: &NodeConfig,
    current: &mut ClusterDocument,
    device_ids: &[DeviceId],
    peer: SocketAddr,
    cluster_id: Uuid,
) -> Result<ClusterDocument, MembershipError> {
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        // A retry after a partial apply may find the change already in
        // the document; then there is nothing to propose.
        let already = current.node(NodeId(config.node_id)).is_some()
            && device_ids.iter().all(|d| current.device(*d).is_some());
        if already {
            Node::save_document_for(config, current)?;
            return Ok(current.clone());
        }
        let next = Node::document_with_this_node(config, current, device_ids);
        match propose(current, &next).await {
            Ok(()) => {
                Node::save_document_for(config, &next)?;
                return Ok(next);
            }
            Err(MembershipError::Superseded { .. })
            | Err(MembershipError::StaleProposal { .. }) => {
                *current = fetch_document(peer, cluster_id).await?;
                Node::save_document_for(config, current)?;
            }
            Err(e) => return Err(e),
        }
    }
    Err(MembershipError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS))
}

/// Startup adoption (18.1.2): compare the saved document with each
/// bootstrap peer's, adopt a higher version, refuse a different cluster,
/// ignore unreachable peers. Returns the document to open with.
pub async fn adopt_from_peers(config: &NodeConfig) -> Result<ClusterDocument, MembershipError> {
    let mut document = Node::load_document_for(config)?.ok_or_else(|| {
        MembershipError::Node(NodeError::Io {
            path: Node::document_path_for(config),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no cluster document; run init-cluster or join first",
            ),
        })
    })?;
    for peer_text in &config.bootstrap_peers {
        let peer: SocketAddr = match peer_text.parse() {
            Ok(address) => address,
            Err(e) => {
                tracing::warn!(peer = %peer_text, error = %e, "bootstrap peer address is not valid; ignored");
                continue;
            }
        };
        match fetch_document(peer, document.cluster_id).await {
            Ok(theirs) if theirs.version > document.version => {
                tracing::info!(%peer, from = document.version, to = theirs.version, "adopting newer cluster document from peer");
                Node::save_document_for(config, &theirs)?;
                document = theirs;
            }
            Ok(_) => {}
            Err(e @ MembershipError::WrongCluster { .. }) => return Err(e),
            Err(e) => {
                tracing::warn!(%peer, error = %e, "bootstrap peer not consulted");
            }
        }
    }
    Ok(document)
}
