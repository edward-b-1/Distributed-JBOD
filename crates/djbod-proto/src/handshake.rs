//! The connection opening (SPEC 19.1.5).
//!
//! Each side sends one `Hello` before anything else. A node refuses the
//! connection if the protocol version is unsupported, the cluster id is
//! not its own, or a node peer holds a different cluster document
//! version. That catches a node or client pointed at the wrong cluster and
//! a node running stale software or configuration.
//!
//! It authenticates nothing. v1 assumes a trusted LAN (3.12); TLS is the
//! planned means of authentication (19.1.6). A shared-secret HMAC proof was
//! built and removed, for the reasons in 6.1.2.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use djbod_core::cluster::NodeId;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerKind {
    Node,
    Client,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub kind: PeerKind,
    /// Present for nodes, absent for clients.
    pub node_id: Option<NodeId>,
    pub cluster_id: Uuid,
    /// The cluster document version this peer holds; 0 for clients.
    pub document_version: u64,
    /// The peer's software build, version and commit (19.1.5). Absent
    /// from builds before it, so `cluster show` can name an older node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The cluster's name from the document, if it has one (6.2.5.3).
    /// Informational: the id is what is checked. Clients send none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_name: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HelloError {
    #[error("peer speaks protocol version {peer}, this node speaks {ours}")]
    ProtocolVersion { peer: u32, ours: u32 },
    #[error("peer belongs to cluster {peer}, this node to {ours}")]
    ClusterId { peer: Uuid, ours: Uuid },
    #[error("peer node holds cluster document version {peer}, this node {ours}")]
    DocumentVersion { peer: u64, ours: u64 },
    #[error("a node peer must send its node id")]
    NodeIdMissing,
}

impl Hello {
    /// The checks a node applies to a peer's `Hello` (19.1.5, 6.2.7).
    /// Clients are not held to the document version.
    pub fn check_against(
        &self,
        our_cluster_id: Uuid,
        our_document_version: u64,
    ) -> Result<(), HelloError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(HelloError::ProtocolVersion {
                peer: self.protocol_version,
                ours: PROTOCOL_VERSION,
            });
        }
        if self.cluster_id != our_cluster_id {
            return Err(HelloError::ClusterId {
                peer: self.cluster_id,
                ours: our_cluster_id,
            });
        }
        if self.kind == PeerKind::Node {
            if self.node_id.is_none() {
                return Err(HelloError::NodeIdMissing);
            }
            if self.document_version != our_document_version {
                return Err(HelloError::DocumentVersion {
                    peer: self.document_version,
                    ours: our_document_version,
                });
            }
        }
        Ok(())
    }
}
