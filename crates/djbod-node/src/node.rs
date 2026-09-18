//! A node's state: its configuration, its devices, and its copy of the
//! cluster document (SPEC 5, 6.2).

use std::collections::{BTreeMap, HashMap};
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
use crate::ulid::VersionGenerator;

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
    #[error(
        "cluster document version {proposed} is not higher than the current version {current}"
    )]
    NotNewer { current: u64, proposed: u64 },
    #[error("cluster document names cluster {proposed}, this node belongs to {ours}")]
    WrongCluster { ours: Uuid, proposed: Uuid },
}

/// A running node's shared state.
pub struct Node {
    config: NodeConfig,
    document: RwLock<ClusterDocument>,
    /// In configuration file order, which is the order an administrator
    /// expects to see them listed.
    devices: Vec<Arc<Device>>,
    devices_by_id: HashMap<DeviceId, Arc<Device>>,
    versions: VersionGenerator,
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

    pub fn versions(&self) -> &VersionGenerator {
        &self.versions
    }

    pub fn device(&self, id: DeviceId) -> Option<Arc<Device>> {
        self.devices_by_id.get(&id).cloned()
    }

    /// This node's devices in configuration file order.
    pub fn devices(&self) -> Vec<Arc<Device>> {
        self.devices.clone()
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
                addresses: vec![config.advertised_address().to_string()],
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
        let mut by_filesystem: BTreeMap<u64, Vec<&Device>> = BTreeMap::new();
        for device in &devices {
            by_filesystem
                .entry(device.filesystem_id())
                .or_default()
                .push(device);
        }
        for group in by_filesystem.values().filter(|g| g.len() > 1) {
            if config.allow_shared_filesystem {
                let paths: Vec<String> = group
                    .iter()
                    .map(|d| d.root().display().to_string())
                    .collect();
                tracing::warn!(
                    devices = %paths.join(", "),
                    "these devices share one filesystem; allow_shared_filesystem is set, so losing that disk loses all of them"
                );
            } else {
                return Err(NodeError::SameFilesystem {
                    a: group[0].root().to_path_buf(),
                    b: group[1].root().to_path_buf(),
                });
            }
        }
        let node_id = NodeId(config.node_id);
        if document.node(node_id).is_none() {
            return Err(NodeError::NotAMember { node: node_id });
        }
        // A cluster may legitimately have fewer active devices than the
        // scheme needs, for example before other nodes join (7.3), but
        // every write will fail until it does not, so say so loudly.
        let active = document
            .devices
            .iter()
            .filter(|d| d.state == DeviceState::Active)
            .count();
        let needed = document.k as usize + document.m as usize;
        if active < needed {
            tracing::warn!(
                active_devices = active,
                needed,
                k = document.k,
                m = document.m,
                "the cluster has fewer active devices than k + m; every write will fail with InsufficientDevices until devices are added"
            );
        }
        let mut ordered = Vec::with_capacity(devices.len());
        let mut by_id = HashMap::with_capacity(devices.len());
        for device in devices {
            if document.device(device.id()).is_none() {
                return Err(NodeError::UnknownDevice {
                    device: device.id(),
                    path: device.root().to_path_buf(),
                });
            }
            let device = Arc::new(device);
            by_id.insert(device.id(), device.clone());
            ordered.push(device);
        }
        Ok(Node {
            config,
            document: RwLock::new(document),
            devices: ordered,
            devices_by_id: by_id,
            versions: VersionGenerator::new(),
        })
    }

    /// Adopt a new cluster document (SPEC 6.2.6, `ApplyClusterConfig`):
    /// it must name this cluster and carry a higher version than the one
    /// held. Versions are produced serially through the first-listed node
    /// (6.2.6), so any higher version is the unique successor and safe to
    /// adopt (6.2.6.1). Saved durably before it takes effect.
    pub fn apply_document(&self, proposed: ClusterDocument) -> Result<(), NodeError> {
        proposed.validate()?;
        let mut current = self.document.write().expect("document lock");
        if proposed.cluster_id != current.cluster_id {
            return Err(NodeError::WrongCluster {
                ours: current.cluster_id,
                proposed: proposed.cluster_id,
            });
        }
        if proposed.version <= current.version {
            return Err(NodeError::NotNewer {
                current: current.version,
                proposed: proposed.version,
            });
        }
        save_document(&Self::document_path(&self.config), &proposed)?;
        tracing::info!(
            from = current.version,
            to = proposed.version,
            nodes = proposed.nodes.len(),
            devices = proposed.devices.len(),
            "cluster document changed"
        );
        *current = proposed;
        Ok(())
    }

    /// The path of this configuration's saved cluster document.
    pub fn document_path_for(config: &NodeConfig) -> PathBuf {
        Self::document_path(config)
    }

    /// Save a document into a configuration's state directory without
    /// opening the node: used by `join` before the node exists and by
    /// startup adoption before the node opens (18.1.1, 18.1.2).
    pub fn save_document_for(
        config: &NodeConfig,
        document: &ClusterDocument,
    ) -> Result<(), NodeError> {
        fs::create_dir_all(&config.state_dir).map_err(|source| NodeError::Io {
            path: config.state_dir.clone(),
            source,
        })?;
        save_document(&Self::document_path(config), document)
    }

    /// Load the saved document, if any.
    pub fn load_document_for(config: &NodeConfig) -> Result<Option<ClusterDocument>, NodeError> {
        let path = Self::document_path(config);
        if !path.exists() {
            return Ok(None);
        }
        load_document(&path).map(Some)
    }

    /// The document that would add this node and its (already initialised)
    /// devices to `current` (18.1.1).
    pub fn document_with_this_node(
        config: &NodeConfig,
        current: &ClusterDocument,
        devices: &[DeviceId],
    ) -> ClusterDocument {
        let node_id = NodeId(config.node_id);
        let mut next = current.clone();
        next.version += 1;
        if next.node(node_id).is_none() {
            next.nodes.push(NodeEntry {
                id: node_id,
                addresses: vec![config.advertised_address().to_string()],
            });
        }
        for device in devices {
            if next.device(*device).is_none() {
                next.devices.push(DeviceEntry {
                    id: *device,
                    node: node_id,
                    state: DeviceState::Active,
                });
            }
        }
        next
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
