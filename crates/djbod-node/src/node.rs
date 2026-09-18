//! A node's state: its configuration, its devices, and its copy of the
//! cluster document (SPEC 5, 6.2).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use djbod_core::cluster::{
    ClusterDocument, ClusterDocumentError, DeviceEntry, DeviceState, IndependenceLevel, NodeEntry,
    NodeId,
};
use djbod_core::device::{Device, DeviceError};
use djbod_core::record::DeviceId;

use crate::config::NodeConfig;

pub const CLUSTER_DOCUMENT_FILE: &str = "cluster.json";

/// Parameters for creating a new cluster (SPEC 6.2.2).
#[derive(Debug, Clone)]
pub struct ClusterParameters {
    pub k: u8,
    pub m: u8,
    pub block_size: u64,
    pub headroom: f64,
}

impl Default for ClusterParameters {
    fn default() -> ClusterParameters {
        ClusterParameters {
            k: 3,
            m: 1,
            block_size: 1 << 20,
            headroom: 0.05,
        }
    }
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("devices {a} and {b} are on the same filesystem; two configured devices must be two disks (SPEC 5.3)")]
    SameFilesystem { a: PathBuf, b: PathBuf },
    #[error("cannot read or write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cluster document at {path} is not valid: {reason}")]
    BadDocument { path: PathBuf, reason: String },
    #[error(transparent)]
    InvalidDocument(#[from] ClusterDocumentError),
    #[error("this node {node} is not in the cluster document")]
    NotAMember { node: NodeId },
    #[error("device {device} at {path} is not in the cluster document")]
    UnknownDevice { device: DeviceId, path: PathBuf },
    #[error("cluster document version {proposed} is not the current version {current} plus one")]
    NotNextVersion { current: u64, proposed: u64 },
    #[error("cluster document names cluster {proposed}, this node belongs to {ours}")]
    WrongCluster { ours: Uuid, proposed: Uuid },
}

/// A running node's shared state.
pub struct Node {
    config: NodeConfig,
    document: RwLock<ClusterDocument>,
    devices: HashMap<DeviceId, Arc<Device>>,
}

impl Node {
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.config.node_id)
    }

    pub fn cluster_id(&self) -> Uuid {
        self.document.read().expect("document lock").cluster_id
    }

    pub fn document(&self) -> ClusterDocument {
        self.document.read().expect("document lock").clone()
    }

    pub fn document_version(&self) -> u64 {
        self.document.read().expect("document lock").version
    }

    pub fn device(&self, id: DeviceId) -> Option<Arc<Device>> {
        self.devices.get(&id).cloned()
    }

    pub fn devices(&self) -> Vec<Arc<Device>> {
        let mut all: Vec<Arc<Device>> = self.devices.values().cloned().collect();
        all.sort_by_key(|d| d.id());
        all
    }

    fn document_path(config: &NodeConfig) -> PathBuf {
        config.state_dir.join(CLUSTER_DOCUMENT_FILE)
    }

    /// Create a new cluster from this node alone (SPEC 6.2, 18.1 for the
    /// first node): initialise every configured directory as a device,
    /// write cluster document version 1, and open the node.
    pub fn init_cluster(
        config: NodeConfig,
        parameters: ClusterParameters,
    ) -> Result<Node, NodeError> {
        let cluster_id = Uuid::new_v4();
        fs::create_dir_all(&config.state_dir).map_err(|source| NodeError::Io {
            path: config.state_dir.clone(),
            source,
        })?;
        let mut devices = Vec::with_capacity(config.devices.len());
        for path in &config.devices {
            devices.push(Device::initialise(path, cluster_id)?);
        }
        let node_id = NodeId(config.node_id);
        let document = ClusterDocument {
            version: 1,
            cluster_id,
            k: parameters.k,
            m: parameters.m,
            block_size: parameters.block_size,
            independence_level: IndependenceLevel::Device,
            headroom: parameters.headroom,
            nodes: vec![NodeEntry {
                id: node_id,
                addresses: vec![config.listen.to_string()],
            }],
            devices: devices
                .iter()
                .map(|d| DeviceEntry {
                    id: d.id(),
                    node: node_id,
                    state: DeviceState::Active,
                })
                .collect(),
        };
        document.validate()?;
        save_document(&Self::document_path(&config), &document)?;
        Self::assemble(config, document, devices)
    }

    /// Open an existing node: its devices and its saved cluster document.
    pub fn open(config: NodeConfig) -> Result<Node, NodeError> {
        let document_path = Self::document_path(&config);
        let document = load_document(&document_path)?;
        let mut devices = Vec::with_capacity(config.devices.len());
        for path in &config.devices {
            devices.push(Device::open(path, Some(document.cluster_id))?);
        }
        let max_age = Duration::from_secs(config.temporary_max_age_secs);
        for device in &devices {
            let removed = device.cleanup_temporaries(max_age)?;
            for path in removed {
                tracing::warn!(path = %path.display(), "removed orphaned temporary file");
            }
        }
        Self::assemble(config, document, devices)
    }

    fn assemble(
        config: NodeConfig,
        document: ClusterDocument,
        devices: Vec<Device>,
    ) -> Result<Node, NodeError> {
        // Two configured paths on one filesystem are one disk (5.3).
        for (i, a) in devices.iter().enumerate() {
            for b in &devices[..i] {
                if a.filesystem_id() == b.filesystem_id() {
                    if config.allow_shared_filesystem {
                        tracing::warn!(
                            a = %a.root().display(),
                            b = %b.root().display(),
                            "devices share a filesystem; allow_shared_filesystem is set, so losing that disk loses both"
                        );
                    } else {
                        return Err(NodeError::SameFilesystem {
                            a: a.root().to_path_buf(),
                            b: b.root().to_path_buf(),
                        });
                    }
                }
            }
        }
        let node_id = NodeId(config.node_id);
        if document.node(node_id).is_none() {
            return Err(NodeError::NotAMember { node: node_id });
        }
        let mut by_id = HashMap::with_capacity(devices.len());
        for device in devices {
            if document.device(device.id()).is_none() {
                return Err(NodeError::UnknownDevice {
                    device: device.id(),
                    path: device.root().to_path_buf(),
                });
            }
            by_id.insert(device.id(), Arc::new(device));
        }
        Ok(Node {
            config,
            document: RwLock::new(document),
            devices: by_id,
        })
    }

    /// Adopt a new cluster document (SPEC 6.2.6, `ApplyClusterConfig`):
    /// it must name this cluster and be exactly the current version plus
    /// one. Saved durably before it takes effect.
    pub fn apply_document(&self, proposed: ClusterDocument) -> Result<(), NodeError> {
        proposed.validate()?;
        let mut current = self.document.write().expect("document lock");
        if proposed.cluster_id != current.cluster_id {
            return Err(NodeError::WrongCluster {
                ours: current.cluster_id,
                proposed: proposed.cluster_id,
            });
        }
        if proposed.version != current.version + 1 {
            return Err(NodeError::NotNextVersion {
                current: current.version,
                proposed: proposed.version,
            });
        }
        save_document(&Self::document_path(&self.config), &proposed)?;
        *current = proposed;
        Ok(())
    }
}

fn save_document(path: &Path, document: &ClusterDocument) -> Result<(), NodeError> {
    let json =
        serde_json::to_string_pretty(document).expect("a cluster document always serializes");
    let temp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        fs::write(&temp, json.as_bytes())?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(dir) = path.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    };
    write().map_err(|source| NodeError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn load_document(path: &Path) -> Result<ClusterDocument, NodeError> {
    let json = fs::read_to_string(path).map_err(|source| NodeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let document: ClusterDocument =
        serde_json::from_str(&json).map_err(|e| NodeError::BadDocument {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    document.validate()?;
    Ok(document)
}
