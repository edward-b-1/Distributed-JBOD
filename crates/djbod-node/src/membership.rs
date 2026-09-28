//! Membership changes that touch this node's own state directory:
//! joining a cluster (SPEC 18.1.1), adding devices to a member (18.1.3),
//! and startup adoption (18.1.2). Each is built on the administration
//! procedures of `djbod_client::admin`, which every other change uses
//! directly.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

use djbod_client::admin::{
    fetch_all_except, fetch_document, propose, propose_skipping, with_node_addresses, AdminError,
    MAX_PROPOSAL_ATTEMPTS,
};
use djbod_core::cluster::{ClusterDocument, DeviceState, NodeId, NodeState};
use djbod_core::device::{Device, DeviceError};
use djbod_core::record::DeviceId;

use crate::config::NodeConfig;
use crate::node::{Node, NodeError};
use crate::transport::{Connector, TlsError, TlsMaterial};

/// What can go wrong with this node's own membership, joining, adding a
/// device, or adopting at startup: the administration step failing, or
/// this node's own files and material.
#[derive(Debug, Error)]
pub enum MembershipError {
    #[error(transparent)]
    Admin(#[from] AdminError),
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("device {path} is already initialised and in the document")]
    AlreadyMember { path: PathBuf },
    #[error("node {node} was removed from this cluster and a removed node is never revived (SPEC 18.2.1); give this machine a new node id and join again")]
    RemovedNode { node: NodeId },
    #[error(
        "device {path} was initialised for this cluster but is not in its document: it was removed; pass --wipe-removed-device to erase it and add it as a new device"
    )]
    RemovedDevice { path: PathBuf, device: DeviceId },
    #[error(
        "the cluster document lists this node at {listed:?}, not at its configured address {configured}, and proposing the change failed: {reason}. Serving there would leave the node where no other node can find it, so it does not start; make every node reachable and start again, or configure the listed address"
    )]
    AddressChangeFailed {
        configured: SocketAddr,
        listed: Vec<String>,
        reason: Box<AdminError>,
    },
}

/// Join this node to an existing cluster (18.1.1). On success the
/// configuration's state directory holds the new document and every
/// configured device is initialised and listed, so `run` may follow.
pub async fn join(
    config: &NodeConfig,
    peer: SocketAddr,
    cluster_id: Uuid,
    wipe_removed_devices: bool,
) -> Result<ClusterDocument, MembershipError> {
    // A joining node does not yet know the cluster's transport; it tries
    // TLS first when it has material (a plain peer accepts nothing else
    // if the transport is tls) and falls back to plain otherwise.
    let connector = connector_for_unknown_transport(config, peer, cluster_id).await?;
    let mut current = fetch_document(&connector, peer, cluster_id).await?;
    if current.transport != djbod_core::cluster::Transport::Plain && config.tls.is_none() {
        return Err(AdminError::TlsRequired {
            transport: current.transport,
        }
        .into());
    }
    // A removed node's id is a tombstone, never revived (18.2.1).
    let node_id = NodeId(config.node_id);
    if current
        .node(node_id)
        .is_some_and(|n| n.state == NodeState::Removed)
    {
        return Err(MembershipError::RemovedNode { node: node_id });
    }
    Node::save_document_for(config, &current)?;
    let mut device_ids = Vec::with_capacity(config.devices.len());
    for path in &config.devices {
        let device = open_or_initialise(path, &current, wipe_removed_devices)?;
        device_ids.push(device.id());
    }
    propose_with_retry(
        &connector,
        config,
        &mut current,
        &device_ids,
        peer,
        cluster_id,
    )
    .await
}

/// The connector a node binary uses before it has read the cluster's
/// transport: TLS when it has material and the peer accepts it, plain
/// otherwise.
async fn connector_for_unknown_transport(
    config: &NodeConfig,
    peer: SocketAddr,
    cluster_id: Uuid,
) -> Result<Connector, MembershipError> {
    if let Some(paths) = &config.tls {
        let material = TlsMaterial::load(paths)?;
        let tls = material.connector();
        if fetch_document(&tls, peer, cluster_id).await.is_ok() {
            return Ok(tls);
        }
    }
    Ok(Connector::plain())
}

/// The connector a node binary uses once it holds a document: as the
/// document's transport says.
pub fn connector_for(
    config: &NodeConfig,
    document: &ClusterDocument,
) -> Result<Connector, MembershipError> {
    if document.transport == djbod_core::cluster::Transport::Plain {
        return Ok(Connector::plain());
    }
    match &config.tls {
        Some(paths) => Ok(TlsMaterial::load(paths)?.connector()),
        None => Err(AdminError::TlsRequired {
            transport: document.transport,
        }
        .into()),
    }
}

/// A device path for `join` or `add-device`: initialise it if it is
/// empty; reuse it if it is already in the document (a retry after a
/// partial failure); and if it was initialised for this cluster but is no
/// longer listed, it was removed (18.2.1, 6.2.6.3): refuse, or erase it
/// and initialise it afresh when asked to (SPEC 6.2.6.3).
fn open_or_initialise(
    path: &Path,
    document: &ClusterDocument,
    wipe_removed: bool,
) -> Result<Device, MembershipError> {
    match Device::open(path, Some(document.cluster_id)) {
        // Listed and not removed: a retry after a partial failure. A
        // tombstone (18.2.1) is a removed device, below.
        Ok(existing)
            if document
                .device(existing.id())
                .is_some_and(|d| d.state != DeviceState::Removed) =>
        {
            Ok(existing)
        }
        Ok(existing) if wipe_removed => {
            tracing::warn!(
                path = %path.display(),
                device = %existing.id(),
                "erasing a removed device; every shard and record it held is gone"
            );
            drop(existing);
            Ok(Device::wipe_and_initialise(path, document.cluster_id)?)
        }
        Ok(existing) => Err(MembershipError::RemovedDevice {
            path: path.to_path_buf(),
            device: existing.id(),
        }),
        Err(DeviceError::NotInitialised { .. }) => {
            Ok(Device::initialise(path, document.cluster_id)?)
        }
        Err(e) => Err(e.into()),
    }
}

/// Initialise `paths` (which must be listed in the configuration) and
/// add them to the document (18.1.3). The node picks them up at its next
/// start.
pub async fn add_devices(
    config: &NodeConfig,
    paths: &[PathBuf],
    peer: SocketAddr,
    cluster_id: Uuid,
    wipe_removed_devices: bool,
) -> Result<ClusterDocument, MembershipError> {
    let connector = connector_for_unknown_transport(config, peer, cluster_id).await?;
    let mut current = fetch_document(&connector, peer, cluster_id).await?;
    let mut device_ids: Vec<DeviceId> = Vec::new();
    for path in paths {
        if let Ok(existing) = Device::open(path, Some(cluster_id)) {
            if current
                .device(existing.id())
                .is_some_and(|d| d.state != DeviceState::Removed)
            {
                return Err(MembershipError::AlreadyMember { path: path.clone() });
            }
        }
        let device = open_or_initialise(path, &current, wipe_removed_devices)?;
        device_ids.push(device.id());
    }
    propose_with_retry(
        &connector,
        config,
        &mut current,
        &device_ids,
        peer,
        cluster_id,
    )
    .await
}

async fn propose_with_retry(
    connector: &Connector,
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
        match propose(connector, current, &next).await {
            Ok(()) => {
                Node::save_document_for(config, &next)?;
                return Ok(next);
            }
            Err(AdminError::Superseded { .. }) | Err(AdminError::StaleProposal { .. }) => {
                *current = fetch_document(connector, peer, cluster_id).await?;
                Node::save_document_for(config, current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS).into())
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
        let connector = connector_for(config, &document)?;
        match fetch_document(&connector, peer, document.cluster_id).await {
            Ok(theirs) if theirs.version > document.version => {
                tracing::info!(%peer, from = document.version, to = theirs.version, "adopting newer cluster document from peer");
                Node::save_document_for(config, &theirs)?;
                document = theirs;
            }
            Ok(_) => {}
            Err(e @ AdminError::WrongCluster { .. }) => return Err(e.into()),
            Err(e) => {
                tracing::warn!(%peer, error = %e, "bootstrap peer not consulted");
            }
        }
    }
    adopt_configured_address(config, document).await
}

/// The startup case of SPEC 18.1.2.1: when the document's entry for this
/// node does not list its configured advertised address, propose a
/// document that lists that address alone. This node is not serving yet,
/// so it is skipped in the proposal as a dead node would be (6.2.6.3 step
/// 2) and saves the result itself. A node not in the document is left for
/// `Node::open` to refuse.
async fn adopt_configured_address(
    config: &NodeConfig,
    mut document: ClusterDocument,
) -> Result<ClusterDocument, MembershipError> {
    let node = NodeId(config.node_id);
    let configured = config.advertised_address();
    let connector = connector_for(config, &document)?;
    for _ in 0..MAX_PROPOSAL_ATTEMPTS {
        let Some(entry) = document.node(node) else {
            return Ok(document);
        };
        if entry.addresses.contains(&configured.to_string()) {
            return Ok(document);
        }
        let listed = entry.addresses.clone();
        let next = with_node_addresses(&document, node, vec![configured.to_string()]);
        match propose_skipping(&connector, &document, &next, Some(node)).await {
            Ok(()) => {
                Node::save_document_for(config, &next)?;
                tracing::info!(
                    from = document.version,
                    to = next.version,
                    listed = ?listed,
                    %configured,
                    "the cluster document now lists this node at its configured address"
                );
                return Ok(next);
            }
            Err(AdminError::Superseded { .. })
            | Err(AdminError::StaleProposal { .. })
            | Err(AdminError::VersionsDiffer(_)) => {
                // Another change won; take the newest document any other
                // node holds and try again from there.
                for report in fetch_all_except(&connector, &document, Some(node)).await {
                    if let Ok(theirs) = report.result {
                        if theirs.version > document.version {
                            Node::save_document_for(config, &theirs)?;
                            document = theirs;
                        }
                    }
                }
            }
            Err(reason) => {
                return Err(MembershipError::AddressChangeFailed {
                    configured,
                    listed,
                    reason: Box::new(reason),
                })
            }
        }
    }
    Err(AdminError::TooManyRetries(MAX_PROPOSAL_ATTEMPTS).into())
}
