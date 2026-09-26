//! A node's state: its configuration, its devices, and its copy of the
//! cluster document (SPEC 5, 6.2).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use djbod_core::cluster::{
    ClusterDocument, ClusterDocumentError, DeviceEntry, DeviceState, IndependenceLevel, NodeEntry,
    NodeId, NodeState, Transport,
};
use djbod_core::device::{Device, DeviceError};
use djbod_core::keyhash::KeyHash;
use djbod_core::record::DeviceId;
use djbod_core::version::VersionId;

use crate::config::NodeConfig;
use crate::transport::{Connector, TlsError, TlsMaterial};
use crate::ulid::VersionGenerator;

pub const CLUSTER_DOCUMENT_FILE: &str = "cluster.json";

/// Parameters for creating a new cluster (SPEC 6.2.2).
#[derive(Debug, Clone)]
pub struct ClusterParameters {
    /// The cluster's name (SPEC 6.2.5.3), or none.
    pub name: Option<String>,
    pub k: u8,
    pub m: u8,
    pub block_size: u64,
    pub headroom: f64,
    pub max_key_bytes: u64,
    pub max_object_bytes: u64,
    pub max_user_metadata_bytes: u64,
    /// The cluster id to create with, for provisioning that must know it
    /// in advance (a compose file, a fleet tool); a fresh UUID if `None`.
    pub cluster_id: Option<Uuid>,
}

impl Default for ClusterParameters {
    fn default() -> ClusterParameters {
        ClusterParameters {
            name: None,
            k: 3,
            m: 1,
            block_size: 1 << 20,
            headroom: 0.05,
            max_key_bytes: djbod_core::cluster::DEFAULT_MAX_KEY_BYTES,
            max_object_bytes: djbod_core::cluster::DEFAULT_MAX_OBJECT_BYTES,
            max_user_metadata_bytes: djbod_core::cluster::DEFAULT_MAX_USER_METADATA_BYTES,
            cluster_id: None,
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
    #[error("this node {node} is not an active member of the cluster: it was removed or never joined (SPEC 18.2.1, 6.2.6.3); a machine that comes back joins with a new node id, and its devices with --wipe-removed-device")]
    NotAMember { node: NodeId },
    #[error("device {device} at {path} is not in the cluster document")]
    UnknownDevice { device: DeviceId, path: PathBuf },
    #[error(
        "cluster document version {proposed} is not higher than the current version {current}"
    )]
    NotNewer { current: u64, proposed: u64 },
    #[error("cluster document names cluster {proposed}, this node belongs to {ours}")]
    WrongCluster { ours: Uuid, proposed: Uuid },
    #[error(
        "the cluster's transport is {transport} but this node has no TLS material; give [tls] paths in the configuration or --tls-cert, --tls-key, --tls-ca (SPEC 19.1.6.2)"
    )]
    TlsRequired { transport: Transport },
    #[error(transparent)]
    Tls(#[from] TlsError),
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
    /// Shard writes in progress on this node, so a second `PutShard` for
    /// the same shard is refused rather than racing the first
    /// (SPEC 20.1.2.1).
    writes_in_flight: Mutex<HashSet<ShardWriteKey>>,
    /// Becomes true when an adopted document no longer lists this node
    /// (18.2.1, 6.2.6.3); the server stops accepting connections.
    removed: tokio::sync::watch::Sender<bool>,
    /// Devices found unavailable at run time (5.6), so the loss is logged
    /// once and its end once, not on every status request.
    unavailable_reported: Mutex<HashSet<DeviceId>>,
    /// Loaded from the configured paths at startup (19.1.6.2).
    tls: Option<Arc<TlsMaterial>>,
    /// Connections accepted since startup, by transport, for status and
    /// tests.
    accepted_plain: AtomicU64,
    accepted_tls: AtomicU64,
}

/// Identifies one shard file being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardWriteKey {
    pub device: DeviceId,
    pub key_hash: KeyHash,
    pub version: VersionId,
    pub shard_index: u8,
}

/// Held for the duration of a shard write; releases the slot on drop.
pub struct ShardWriteGuard {
    node: Arc<Node>,
    key: ShardWriteKey,
}

impl Drop for ShardWriteGuard {
    fn drop(&mut self) {
        self.node
            .writes_in_flight
            .lock()
            .expect("writes lock")
            .remove(&self.key);
    }
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

    /// Claim a shard for writing. `None` if a write for the same shard on
    /// the same device is already in progress.
    pub fn begin_shard_write(self: &Arc<Self>, key: ShardWriteKey) -> Option<ShardWriteGuard> {
        let mut in_flight = self.writes_in_flight.lock().expect("writes lock");
        if !in_flight.insert(key) {
            return None;
        }
        Some(ShardWriteGuard {
            node: self.clone(),
            key,
        })
    }

    pub fn device(&self, id: DeviceId) -> Option<Arc<Device>> {
        self.devices_by_id.get(&id).cloned()
    }

    /// This node's devices in configuration file order.
    pub fn devices(&self) -> Vec<Arc<Device>> {
        self.devices.clone()
    }

    /// Devices the document lists for this node that were not opened at
    /// startup (5.6): their path is missing, empty, or not configured.
    pub fn unavailable_devices(&self) -> Vec<DeviceId> {
        let document = self.document.read().expect("document lock");
        document
            .devices
            .iter()
            .filter(|d| {
                d.node == self.id()
                    && d.state != DeviceState::Removed
                    && !self.devices_by_id.contains_key(&d.id)
            })
            .map(|d| d.id)
            .collect()
    }

    /// Record that `device` was found unavailable, or available again.
    /// Returns true when that is a change, so the caller logs it once.
    pub fn note_availability(&self, device: DeviceId, available: bool) -> bool {
        let mut reported = self.unavailable_reported.lock().expect("availability lock");
        if available {
            reported.remove(&device)
        } else {
            reported.insert(device)
        }
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
        let cluster_id = parameters.cluster_id.unwrap_or_else(Uuid::new_v4);
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
            name: parameters.name.clone(),
            k: parameters.k,
            m: parameters.m,
            block_size: parameters.block_size,
            independence_level: IndependenceLevel::Device,
            headroom: parameters.headroom,
            max_key_bytes: parameters.max_key_bytes,
            max_object_bytes: parameters.max_object_bytes,
            max_user_metadata_bytes: parameters.max_user_metadata_bytes,
            transport: Transport::Plain,
            nodes: vec![NodeEntry {
                id: node_id,
                addresses: vec![config.advertised_address().to_string()],
                label: None,
                state: NodeState::Active,
            }],
            devices: devices
                .iter()
                .map(|d| DeviceEntry {
                    id: d.id(),
                    node: node_id,
                    state: DeviceState::Active,
                    label: None,
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
        if document.transport != Transport::Plain && config.tls.is_none() {
            return Err(NodeError::TlsRequired {
                transport: document.transport,
            });
        }
        let mut devices = Vec::with_capacity(config.devices.len());
        for path in &config.devices {
            match Device::open(path, Some(document.cluster_id)) {
                Ok(device) => devices.push(device),
                // A missing directory or an empty mount point: the disk is
                // not there. The device it should hold is reported
                // unavailable (5.6) rather than the node refusing to start.
                Err(e @ DeviceError::NotInitialised { .. }) => {
                    tracing::warn!(path = %path.display(), %e, "configured device path cannot be opened; the device it held is unavailable");
                }
                Err(e @ DeviceError::Io { .. }) if matches!(&e, DeviceError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound) =>
                {
                    tracing::warn!(path = %path.display(), %e, "configured device path cannot be opened; the device it held is unavailable");
                }
                Err(e) => return Err(e.into()),
            }
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
        let tls = match &config.tls {
            Some(paths) => Some(Arc::new(TlsMaterial::load(paths)?)),
            None => None,
        };
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
        if !document
            .node(node_id)
            .is_some_and(|n| n.state == NodeState::Active)
        {
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
        for entry in document.devices.iter().filter(|d| {
            d.node == node_id && d.state != DeviceState::Removed && !by_id.contains_key(&d.id)
        }) {
            tracing::warn!(
                device = %entry.id,
                label = entry.label.as_deref().unwrap_or("-"),
                "device listed for this node was not opened: no configured path holds it; it is unavailable (SPEC 5.6)"
            );
        }
        Ok(Node {
            config,
            document: RwLock::new(document),
            devices: ordered,
            devices_by_id: by_id,
            versions: VersionGenerator::new(),
            writes_in_flight: Mutex::new(HashSet::new()),
            removed: tokio::sync::watch::Sender::new(false),
            unavailable_reported: Mutex::new(HashSet::new()),
            tls,
            accepted_plain: AtomicU64::new(0),
            accepted_tls: AtomicU64::new(0),
        })
    }

    /// How long a receiver waits for the next frame of a stream (10.12).
    pub fn stream_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.config.stream_idle_timeout_secs.max(1))
    }

    /// This node's TLS material, if configured.
    pub fn tls(&self) -> Option<&Arc<TlsMaterial>> {
        self.tls.as_ref()
    }

    /// How this node connects to its peers: TLS whenever the document's
    /// transport is not `plain` (19.1.6.4).
    pub fn connector(&self) -> Result<Connector, NodeError> {
        let transport = self.document.read().expect("document lock").transport;
        match (transport, &self.tls) {
            (Transport::Plain, _) => Ok(Connector::plain()),
            (_, Some(material)) => Ok(material.connector()),
            (transport, None) => Err(NodeError::TlsRequired { transport }),
        }
    }

    pub fn count_accepted(&self, tls: bool) {
        let counter = if tls {
            &self.accepted_tls
        } else {
            &self.accepted_plain
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Connections accepted since startup: (plain, TLS).
    pub fn connections_accepted(&self) -> (u64, u64) {
        (
            self.accepted_plain.load(Ordering::Relaxed),
            self.accepted_tls.load(Ordering::Relaxed),
        )
    }

    /// Whether an adopted document has dropped this node.
    pub fn is_removed(&self) -> bool {
        *self.removed.borrow()
    }

    /// Resolves once this node has been removed from the cluster.
    pub async fn removed(&self) {
        let mut receiver = self.removed.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
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
        if proposed.transport != Transport::Plain && self.tls.is_none() {
            return Err(NodeError::TlsRequired {
                transport: proposed.transport,
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
        let still_active = proposed
            .node(self.id())
            .is_some_and(|n| n.state == NodeState::Active);
        *current = proposed;
        drop(current);
        if !still_active {
            tracing::warn!(
                "this node is removed in the new cluster document and will stop serving"
            );
            self.removed.send_replace(true);
        }
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
                label: None,
                state: NodeState::Active,
            });
        }
        for device in devices {
            if next.device(*device).is_none() {
                next.devices.push(DeviceEntry {
                    id: *device,
                    node: node_id,
                    state: DeviceState::Active,
                    label: None,
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
