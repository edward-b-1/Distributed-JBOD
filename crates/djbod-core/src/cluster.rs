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
/// Default sanity limit on key length: 16 KiB (SPEC 9.1.5).
pub const DEFAULT_MAX_KEY_BYTES: u64 = 16 * 1024;
/// Largest key length a document may allow: 1 MiB, so that a key always
/// fits comfortably inside one protocol frame (SPEC 6.2.4).
pub const LIMIT_MAX_KEY_BYTES: u64 = 1024 * 1024;
/// Default maximum object size: 1 TiB (SPEC 9.3.1).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 1 << 40;
/// Default limit on a record's user metadata, as the sum of its keys' and
/// values' lengths: 10 MiB (SPEC 9.4.2).
pub const DEFAULT_MAX_USER_METADATA_BYTES: u64 = 10 * 1024 * 1024;
/// Largest user metadata limit a document may set: 48 MiB, so that one
/// record always fits inside one protocol frame with room to spare
/// (SPEC 6.2.4, 9.4.2.1.1).
pub const LIMIT_MAX_USER_METADATA_BYTES: u64 = 48 * 1024 * 1024;

fn default_max_user_metadata_bytes() -> u64 {
    DEFAULT_MAX_USER_METADATA_BYTES
}

fn default_max_key_bytes() -> u64 {
    DEFAULT_MAX_KEY_BYTES
}

fn default_max_object_bytes() -> u64 {
    DEFAULT_MAX_OBJECT_BYTES
}

/// A node's identity in the cluster document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node {}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeEntry {
    pub id: NodeId,
    /// `host:port` strings the node listens on.
    pub addresses: Vec<String>,
    /// An administrator-chosen name shown beside the UUID (SPEC 6.2.5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How connections are made and accepted (SPEC 19.1.6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Plain TCP everywhere; no TLS material required.
    #[default]
    Plain,
    /// Nodes speak mutual TLS to each other; clients may use either.
    TlsOptional,
    /// TLS only; clients must present a certificate.
    Tls,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Transport::Plain => "plain",
            Transport::TlsOptional => "tls-optional",
            Transport::Tls => "tls",
        })
    }
}

impl std::str::FromStr for Transport {
    type Err = String;
    fn from_str(text: &str) -> Result<Transport, String> {
        match text {
            "plain" => Ok(Transport::Plain),
            "tls-optional" => Ok(Transport::TlsOptional),
            "tls" => Ok(Transport::Tls),
            other => Err(format!(
                "{other:?} is not a transport; use plain, tls-optional, or tls"
            )),
        }
    }
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
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    pub id: DeviceId,
    pub node: NodeId,
    pub state: DeviceState,
    /// An administrator-chosen name shown beside the UUID (SPEC 6.2.5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Longest device label, in bytes (SPEC 6.2.5.1).
pub const MAX_LABEL_BYTES: usize = 128;

/// Whether `label` may name a device: 1 to 128 bytes of printable text
/// with no whitespace, and not something a UUID could be mistaken for.
pub fn validate_label(label: &str) -> Result<(), ClusterDocumentError> {
    if label.is_empty() || label.len() > MAX_LABEL_BYTES {
        return Err(ClusterDocumentError::BadLabel {
            label: label.to_string(),
            reason: format!("must be 1 to {MAX_LABEL_BYTES} bytes"),
        });
    }
    if label.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ClusterDocumentError::BadLabel {
            label: label.to_string(),
            reason: "must not contain whitespace or control characters".to_string(),
        });
    }
    if Uuid::parse_str(label).is_ok() {
        return Err(ClusterDocumentError::BadLabel {
            label: label.to_string(),
            reason: "looks like a UUID".to_string(),
        });
    }
    Ok(())
}

/// A cluster name (SPEC 6.2.5.3) is text for people and nothing else,
/// so spaces are allowed: 1 to 128 bytes, no control characters, and no
/// leading or trailing whitespace, which would be invisible.
pub fn validate_cluster_name(name: &str) -> Result<(), ClusterDocumentError> {
    if name.is_empty() || name.len() > MAX_LABEL_BYTES {
        return Err(ClusterDocumentError::BadClusterName {
            name: name.to_string(),
            reason: format!("must be 1 to {MAX_LABEL_BYTES} bytes"),
        });
    }
    if name.chars().any(char::is_control) {
        return Err(ClusterDocumentError::BadClusterName {
            name: name.to_string(),
            reason: "must not contain control characters".to_string(),
        });
    }
    if name.trim() != name {
        return Err(ClusterDocumentError::BadClusterName {
            name: name.to_string(),
            reason: "must not start or end with whitespace".to_string(),
        });
    }
    Ok(())
}

/// The only independence level v1 accepts (SPEC 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndependenceLevel {
    Device,
}

/// A document with a field this build does not know is refused rather
/// than read without it (SPEC 6.2.6.4): a node that dropped the field
/// would hold the same version as its peers with different content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterDocument {
    /// Monotonically increasing; every change is a new version (6.2.1).
    pub version: u64,
    pub cluster_id: Uuid,
    /// An administrator-chosen name shown beside the id (SPEC 6.2.5.3);
    /// the id stays the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub k: u8,
    pub m: u8,
    /// Shard block size B, in bytes.
    pub block_size: u64,
    pub independence_level: IndependenceLevel,
    /// Fraction of each device's capacity kept free (5.5).
    pub headroom: f64,
    /// Sanity limit on key length in bytes (9.1.5). Absent in documents
    /// written before it existed, which means the default.
    #[serde(default = "default_max_key_bytes")]
    pub max_key_bytes: u64,
    /// Maximum object size in bytes (9.3.1). Absent means the default.
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
    /// Limit on a record's user metadata, keys and values together, in
    /// bytes (9.4.2). Absent means the default.
    #[serde(default = "default_max_user_metadata_bytes")]
    pub max_user_metadata_bytes: u64,
    /// Plain or TLS (19.1.6.4). Absent means `plain`.
    #[serde(default)]
    pub transport: Transport,
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
    #[error("max_key_bytes {0} must be between 1 and {LIMIT_MAX_KEY_BYTES}")]
    BadKeyLimit(u64),
    #[error("max_object_bytes must be at least 1")]
    BadObjectLimit,
    #[error("max_user_metadata_bytes {0} must be between 1 and {LIMIT_MAX_USER_METADATA_BYTES}")]
    BadMetadataLimit(u64),
    #[error("node {0:?} appears more than once")]
    DuplicateNode(NodeId),
    #[error("node {0:?} lists no address; a node entry needs at least one")]
    NoAddresses(NodeId),
    #[error("node {node:?} address {address:?} is not an IP address and port: {reason}")]
    BadAddress {
        node: NodeId,
        address: String,
        reason: String,
    },
    #[error("address {address} is listed for more than one node")]
    DuplicateAddress { address: String },
    #[error("device {0:?} appears more than once")]
    DuplicateDevice(DeviceId),
    #[error("label {label:?} is not usable: {reason}")]
    BadLabel { label: String, reason: String },
    #[error("cluster name {name:?} is not usable: {reason}")]
    BadClusterName { name: String, reason: String },
    #[error("label {label:?} is used by more than one device")]
    DuplicateLabel { label: String },
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
        if !(1..=LIMIT_MAX_KEY_BYTES).contains(&self.max_key_bytes) {
            return Err(ClusterDocumentError::BadKeyLimit(self.max_key_bytes));
        }
        if self.max_object_bytes == 0 {
            return Err(ClusterDocumentError::BadObjectLimit);
        }
        if !(1..=LIMIT_MAX_USER_METADATA_BYTES).contains(&self.max_user_metadata_bytes) {
            return Err(ClusterDocumentError::BadMetadataLimit(
                self.max_user_metadata_bytes,
            ));
        }
        let mut node_ids: Vec<NodeId> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if node_ids.contains(&node.id) {
                return Err(ClusterDocumentError::DuplicateNode(node.id));
            }
            node_ids.push(node.id);
        }
        // Addresses (6.2.5.2): one or more per node, each an IP address
        // and port, none listed twice anywhere in the document.
        let mut addresses: Vec<&str> = Vec::new();
        for node in &self.nodes {
            if node.addresses.is_empty() {
                return Err(ClusterDocumentError::NoAddresses(node.id));
            }
            for address in &node.addresses {
                if let Err(e) = address.parse::<std::net::SocketAddr>() {
                    return Err(ClusterDocumentError::BadAddress {
                        node: node.id,
                        address: address.clone(),
                        reason: e.to_string(),
                    });
                }
                if addresses.contains(&address.as_str()) {
                    return Err(ClusterDocumentError::DuplicateAddress {
                        address: address.clone(),
                    });
                }
                addresses.push(address);
            }
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
        // Labels, once every device is known to be listed once. Device
        // labels and node labels are separate namespaces: a command that
        // takes a device resolves among devices, one that takes a node
        // among nodes.
        let mut labels: Vec<&str> = Vec::new();
        for device in &self.devices {
            if let Some(label) = &device.label {
                validate_label(label)?;
                if labels.contains(&label.as_str()) {
                    return Err(ClusterDocumentError::DuplicateLabel {
                        label: label.clone(),
                    });
                }
                labels.push(label);
            }
        }
        if let Some(name) = &self.name {
            validate_cluster_name(name)?;
        }
        let mut node_labels: Vec<&str> = Vec::new();
        for node in &self.nodes {
            if let Some(label) = &node.label {
                validate_label(label)?;
                if node_labels.contains(&label.as_str()) {
                    return Err(ClusterDocumentError::DuplicateLabel {
                        label: label.clone(),
                    });
                }
                node_labels.push(label);
            }
        }
        Ok(())
    }

    /// The cluster for people: `name (id)`, or the id alone when unnamed.
    pub fn title(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.cluster_id),
            None => self.cluster_id.to_string(),
        }
    }

    pub fn scheme(&self) -> Result<Scheme, SchemeError> {
        Scheme::new(self.k, self.m)
    }

    pub fn node(&self, id: NodeId) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The node with this label, if any (node labels are unique).
    pub fn node_by_label(&self, label: &str) -> Option<&NodeEntry> {
        self.nodes
            .iter()
            .find(|n| n.label.as_deref() == Some(label))
    }

    /// The node named by a UUID or by a label.
    pub fn node_by_name(&self, name: &str) -> Option<&NodeEntry> {
        match Uuid::parse_str(name) {
            Ok(uuid) => self.node(NodeId(uuid)),
            Err(_) => self.node_by_label(name),
        }
    }

    /// A node's label if it has one, else its UUID, for messages.
    pub fn node_name(&self, id: NodeId) -> String {
        match self.node(id).and_then(|n| n.label.as_deref()) {
            Some(label) => label.to_string(),
            None => id.0.to_string(),
        }
    }

    /// The device with this label, if any (labels are unique).
    pub fn device_by_label(&self, label: &str) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|d| d.label.as_deref() == Some(label))
    }

    /// The device named by a UUID or by a label.
    pub fn device_by_name(&self, name: &str) -> Option<&DeviceEntry> {
        match Uuid::parse_str(name) {
            Ok(uuid) => self.device(DeviceId(uuid)),
            Err(_) => self.device_by_label(name),
        }
    }

    /// A device's label if it has one, else its UUID, for messages.
    pub fn device_name(&self, id: DeviceId) -> String {
        match self.device(id).and_then(|d| d.label.as_deref()) {
            Some(label) => label.to_string(),
            None => id.0.to_string(),
        }
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
            name: None,
            k: 3,
            m: 1,
            block_size: 1 << 20,
            independence_level: IndependenceLevel::Device,
            headroom: 0.05,
            max_key_bytes: DEFAULT_MAX_KEY_BYTES,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_user_metadata_bytes: DEFAULT_MAX_USER_METADATA_BYTES,
            transport: Transport::Plain,
            nodes: vec![
                NodeEntry {
                    id: node_a,
                    addresses: vec!["10.0.0.1:7000".to_string()],
                    label: Some("nas1".to_string()),
                },
                NodeEntry {
                    id: node_b,
                    addresses: vec!["10.0.0.2:7000".to_string()],
                    label: None,
                },
            ],
            devices: vec![
                DeviceEntry {
                    id: DeviceId(Uuid::from_u128(1)),
                    node: node_a,
                    state: DeviceState::Active,
                    label: Some("nas1-bay0".to_string()),
                },
                DeviceEntry {
                    id: DeviceId(Uuid::from_u128(2)),
                    node: node_b,
                    state: DeviceState::Draining,
                    label: None,
                },
            ],
        }
    }

    #[test]
    fn a_field_this_build_does_not_know_is_refused_at_every_level() {
        let mut json = serde_json::to_value(sample()).expect("to json");
        let refused = |json: &serde_json::Value| {
            let error = serde_json::from_value::<ClusterDocument>(json.clone())
                .expect_err("refused")
                .to_string();
            assert!(error.contains("unknown field `colour`"), "{error}");
        };
        let mut with_extra = json.clone();
        with_extra["colour"] = "blue".into();
        refused(&with_extra);
        let mut with_extra = json.clone();
        with_extra["nodes"][0]["colour"] = "blue".into();
        refused(&with_extra);
        with_extra = json.clone();
        with_extra["devices"][0]["colour"] = "blue".into();
        refused(&with_extra);
        // Without the extra field the same text reads back.
        json["nodes"][0]["label"] = "nas1".into();
        let document: ClusterDocument = serde_json::from_value(json).expect("read");
        assert_eq!(document.nodes[0].label.as_deref(), Some("nas1"));
    }

    #[test]
    fn a_cluster_name_is_text_with_spaces_and_shows_beside_the_id() {
        let mut document = sample();
        assert_eq!(document.title(), document.cluster_id.to_string());
        document.name = Some("Home NAS".to_string());
        document.validate().expect("a usable name");
        assert_eq!(
            document.title(),
            format!("Home NAS ({})", document.cluster_id)
        );
        for bad in ["", " padded", "padded ", "two\nlines", &"x".repeat(129)] {
            document.name = Some(bad.to_string());
            assert!(
                matches!(
                    document.validate(),
                    Err(ClusterDocumentError::BadClusterName { .. })
                ),
                "{bad:?}"
            );
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
    fn size_limits_default_when_absent_and_are_bounded() {
        // A document written before the limits existed still parses, at
        // the defaults (9.1.5, 9.3.1).
        let mut value: serde_json::Value =
            serde_json::to_value(sample()).expect("document serializes");
        let fields = value.as_object_mut().expect("object");
        fields.remove("max_key_bytes");
        fields.remove("max_object_bytes");
        fields.remove("max_user_metadata_bytes");
        let parsed: ClusterDocument = serde_json::from_value(value).expect("parses without them");
        assert_eq!(parsed.max_key_bytes, DEFAULT_MAX_KEY_BYTES);
        assert_eq!(parsed.max_object_bytes, DEFAULT_MAX_OBJECT_BYTES);
        assert_eq!(
            parsed.max_user_metadata_bytes,
            DEFAULT_MAX_USER_METADATA_BYTES
        );
        parsed.validate().expect("valid");

        let mut doc = sample();
        doc.max_key_bytes = 0;
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::BadKeyLimit(0))
        ));
        doc.max_key_bytes = LIMIT_MAX_KEY_BYTES + 1;
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::BadKeyLimit(_))
        ));
        let mut doc = sample();
        doc.max_object_bytes = 0;
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::BadObjectLimit)
        ));
        let mut doc = sample();
        doc.max_user_metadata_bytes = LIMIT_MAX_USER_METADATA_BYTES + 1;
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::BadMetadataLimit(_))
        ));
    }

    #[test]
    fn transport_defaults_to_plain_and_uses_kebab_case_names() {
        let mut value: serde_json::Value =
            serde_json::to_value(sample()).expect("document serializes");
        value.as_object_mut().expect("object").remove("transport");
        let parsed: ClusterDocument = serde_json::from_value(value).expect("parses without it");
        assert_eq!(parsed.transport, Transport::Plain);
        let mut doc = sample();
        doc.transport = Transport::TlsOptional;
        let json = serde_json::to_string(&doc).expect("serializes");
        assert!(json.contains("\"transport\":\"tls-optional\""), "{json}");
        assert_eq!("tls".parse::<Transport>(), Ok(Transport::Tls));
        assert!("optional".parse::<Transport>().is_err());
        assert_eq!(Transport::TlsOptional.to_string(), "tls-optional");
    }

    #[test]
    fn node_labels_follow_the_same_rules_in_their_own_namespace() {
        let doc = sample();
        assert_eq!(
            doc.node_by_label("nas1").map(|n| n.id),
            Some(NodeId(Uuid::from_u128(0xA)))
        );
        assert_eq!(
            doc.node_by_name(&Uuid::from_u128(0xB).to_string())
                .map(|n| n.id),
            Some(NodeId(Uuid::from_u128(0xB)))
        );
        assert_eq!(doc.node_name(NodeId(Uuid::from_u128(0xA))), "nas1");
        assert_eq!(
            doc.node_name(NodeId(Uuid::from_u128(0xB))),
            Uuid::from_u128(0xB).to_string()
        );
        // A node and a device may share a label: different namespaces.
        let mut doc = sample();
        doc.nodes[1].label = Some("nas1-bay0".to_string());
        doc.validate().expect("node and device labels are separate");
        // Two nodes may not.
        let mut doc = sample();
        doc.nodes[1].label = Some("nas1".to_string());
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::DuplicateLabel { .. })
        ));
        let mut doc = sample();
        doc.nodes[1].label = Some("has space".to_string());
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::BadLabel { .. })
        ));
    }

    #[test]
    fn labels_are_optional_unique_and_checked() {
        let doc = sample();
        doc.validate().expect("valid");
        assert_eq!(
            doc.device_by_label("nas1-bay0").map(|d| d.id),
            Some(DeviceId(Uuid::from_u128(1)))
        );
        assert_eq!(
            doc.device_by_name("nas1-bay0").map(|d| d.id),
            Some(DeviceId(Uuid::from_u128(1)))
        );
        assert_eq!(
            doc.device_by_name(&Uuid::from_u128(2).to_string())
                .map(|d| d.id),
            Some(DeviceId(Uuid::from_u128(2)))
        );
        assert_eq!(doc.device_name(DeviceId(Uuid::from_u128(1))), "nas1-bay0");
        assert_eq!(
            doc.device_name(DeviceId(Uuid::from_u128(2))),
            Uuid::from_u128(2).to_string()
        );
        let json = serde_json::to_string(&doc).expect("serializes");
        assert_eq!(
            json.matches("\"label\"").count(),
            2,
            "one device and one node are labelled; absent labels are omitted"
        );

        let mut doc = sample();
        doc.devices[1].label = Some("nas1-bay0".to_string());
        assert!(matches!(
            doc.validate(),
            Err(ClusterDocumentError::DuplicateLabel { .. })
        ));
        for bad in [
            "",
            "has space",
            &"x".repeat(129),
            &Uuid::from_u128(9).to_string(),
        ] {
            let mut doc = sample();
            doc.devices[1].label = Some(bad.to_string());
            assert!(
                matches!(doc.validate(), Err(ClusterDocumentError::BadLabel { .. })),
                "{bad:?} should be refused"
            );
        }
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
