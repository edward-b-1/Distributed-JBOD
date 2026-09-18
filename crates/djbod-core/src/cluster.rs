//! The cluster-wide document, SPEC 6.2: the one piece of configuration
//! every node holds an identical copy of.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::erasure::{Scheme, SchemeError};
use crate::record::DeviceId;

/// Smallest permitted shard block size, in bytes: 64 KiB (SPEC 6.2.4).
pub const MIN_BLOCK_SIZE_BYTES: u64 = 64 * 1024;
/// Largest permitted shard block size, in bytes: 64 MiB (SPEC 6.2.4).
pub const MAX_BLOCK_SIZE_BYTES: u64 = 64 * 1024 * 1024;

/// A node's identity in the cluster document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeEntry {
    pub id: NodeId,
    /// `host:port` strings the node listens on.
    pub addresses: Vec<String>,
}

/// SPEC 6.2.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceState {
    Active,
    Draining,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: DeviceId,
    pub node: NodeId,
    pub state: DeviceState,
}

/// The only independence level v1 accepts (SPEC 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndependenceLevel {
    Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterDocument {
    /// Monotonically increasing; every change is a new version (6.2.1).
    pub version: u64,
    pub cluster_id: Uuid,
    pub k: u8,
    pub m: u8,
    /// Shard block size B, in bytes.
    pub block_size: u64,
    pub independence_level: IndependenceLevel,
    /// Fraction of each device's capacity kept free (5.5).
    pub headroom: f64,
    pub nodes: Vec<NodeEntry>,
    pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClusterDocumentError {
    #[error("invalid scheme: {0}")]
    InvalidScheme(#[from] SchemeError),
    #[error(
        "block size {0} must be a multiple of 4096 between {MIN_BLOCK_SIZE_BYTES} and {MAX_BLOCK_SIZE_BYTES}"
    )]
    BadBlockSize(u64),
    #[error("headroom must be between 0 and 0.5")]
    BadHeadroom,
    #[error("node {0:?} appears more than once")]
    DuplicateNode(NodeId),
    #[error("device {0:?} appears more than once")]
    DuplicateDevice(DeviceId),
    #[error("device {device:?} names node {node:?}, which is not in the document")]
    UnknownNode { device: DeviceId, node: NodeId },
}

impl ClusterDocument {
    /// The sanity checks of SPEC 6.2.4, applied whenever a document is
    /// created or received.
    pub fn validate(&self) -> Result<(), ClusterDocumentError> {
        Scheme::new(self.k, self.m)?;
        if !self.block_size.is_multiple_of(4096)
            || self.block_size < MIN_BLOCK_SIZE_BYTES
            || self.block_size > MAX_BLOCK_SIZE_BYTES
        {
            return Err(ClusterDocumentError::BadBlockSize(self.block_size));
        }
        if !(0.0..=0.5).contains(&self.headroom) || self.headroom.is_nan() {
            return Err(ClusterDocumentError::BadHeadroom);
        }
        let mut node_ids: Vec<NodeId> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if node_ids.contains(&node.id) {
                return Err(ClusterDocumentError::DuplicateNode(node.id));
            }
            node_ids.push(node.id);
        }
        let mut device_ids: Vec<DeviceId> = Vec::with_capacity(self.devices.len());
        for device in &self.devices {
            if device_ids.contains(&device.id) {
                return Err(ClusterDocumentError::DuplicateDevice(device.id));
            }
            device_ids.push(device.id);
            if !node_ids.contains(&device.node) {
                return Err(ClusterDocumentError::UnknownNode {
                    device: device.id,
                    node: device.node,
                });
            }
        }
        Ok(())
    }

    pub fn scheme(&self) -> Result<Scheme, SchemeError> {
        Scheme::new(self.k, self.m)
    }

    pub fn node(&self, id: NodeId) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn device(&self, id: DeviceId) -> Option<&DeviceEntry> {
        self.devices.iter().find(|d| d.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ClusterDocument {
        let node_a = NodeId(Uuid::from_u128(0xA));
        let node_b = NodeId(Uuid::from_u128(0xB));
        ClusterDocument {
            version: 1,
            cluster_id: Uuid::from_u128(0xC1),
            k: 3,
            m: 1,
            block_size: 1 << 20,
            independence_level: IndependenceLevel::Device,
            headroom: 0.05,
            nodes: vec![
                NodeEntry {
                    id: node_a,
                    addresses: vec!["10.0.0.1:7000".to_string()],
                },
                NodeEntry {
                    id: node_b,
                    addresses: vec!["10.0.0.2:7000".to_string()],
                },
            ],
            devices: vec![
                DeviceEntry {
                    id: DeviceId(Uuid::from_u128(1)),
                    node: node_a,
                    state: DeviceState::Active,
                },
                DeviceEntry {
                    id: DeviceId(Uuid::from_u128(2)),
                    node: node_b,
                    state: DeviceState::Draining,
                },
            ],
        }
    }

    #[test]
    fn sample_is_valid_and_lookups_work() {
        let doc = sample();
        doc.validate().expect("sample should be valid");
        assert_eq!(doc.scheme().expect("scheme").total_shards(), 4);
        assert!(doc.node(NodeId(Uuid::from_u128(0xA))).is_some());
        assert_eq!(
            doc.device(DeviceId(Uuid::from_u128(2))).map(|d| d.state),
            Some(DeviceState::Draining)
        );
        assert!(doc.device(DeviceId(Uuid::from_u128(9))).is_none());
    }

    #[test]
    fn sanity_limits_are_enforced() {
        let mut doc = sample();
        doc.k = 0;
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::InvalidScheme(_))
        ));

        let mut doc = sample();
        doc.block_size = 4096;
        assert_eq!(
            doc.validate(),
            Err(ClusterDocumentError::BadBlockSize(4096))
        );
        doc.block_size = (1 << 20) + 1;
        assert_eq!(
            doc.validate(),
            Err(ClusterDocumentError::BadBlockSize((1 << 20) + 1))
        );

        let mut doc = sample();
        doc.headroom = 0.9;
        assert_eq!(doc.validate(), Err(ClusterDocumentError::BadHeadroom));

        let mut doc = sample();
        doc.nodes.push(doc.nodes[0].clone());
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::DuplicateNode(_))
        ));

        let mut doc = sample();
        doc.devices.push(doc.devices[0].clone());
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::DuplicateDevice(_))
        ));

        let mut doc = sample();
        doc.devices[0].node = NodeId(Uuid::from_u128(0xF));
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::UnknownNode { .. })
        ));
    }

    #[test]
    fn json_round_trips_with_lowercase_states() {
        let doc = sample();
        let json = serde_json::to_string_pretty(&doc).expect("serialize");
        assert!(json.contains("\"state\": \"draining\""));
        assert!(json.contains("\"independence_level\": \"device\""));
        let parsed: ClusterDocument = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, doc);
    }
}
