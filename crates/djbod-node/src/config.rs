//! The per-node configuration file (SPEC 6.1.1), TOML.
//!
//! ```toml
//! node_id = "2f1c6c3e-..."          # UUID, generated once per node
//! listen = "0.0.0.0:5263"           # optional; default port 5263
//! advertise = "10.0.0.1:5263"       # optional; the address other nodes use to reach
//!                                   # this one; defaults to `listen`. Changing it
//!                                   # moves the node in the cluster document at
//!                                   # the next start (SPEC 18.1.2.1)
//! state_dir = "/var/lib/djbod"      # holds this node's copy of the cluster document
//! devices = ["/mnt/disk0/data", "/mnt/disk1/data"]
//! bootstrap_peers = ["10.0.0.2:5263"]   # optional; empty for the first node
//! allow_shared_filesystem = false   # optional; true only for tests and experiments
//!
//! [tls]                             # optional; paths only, never key material (SPEC 19.1.6.2)
//! cert = "/etc/djbod/node.crt"      # this node's certificate, with an IP SAN for its address
//! key = "/etc/djbod/node.key"       # owner-readable only
//! ca = "/etc/djbod/ca.crt"          # the cluster's authority, or a bundle
//! ```
//!
//! Every setting can also be given to `djbod-node` as an argument or an
//! environment variable, which take precedence in that order over the
//! file (SPEC 20.6): `--node-id`/`DJBOD_NODE_ID`, `--listen`/`DJBOD_LISTEN`,
//! `--advertise`/`DJBOD_ADVERTISE`, `--state-dir`/`DJBOD_STATE_DIR`,
//! `--device` (repeatable)/`DJBOD_DEVICES` (comma-separated),
//! `--bootstrap-peer`/`DJBOD_BOOTSTRAP_PEERS`,
//! `--temporary-max-age-secs`/`DJBOD_TEMPORARY_MAX_AGE_SECS`,
//! `--stream-idle-timeout-secs`/`DJBOD_STREAM_IDLE_TIMEOUT_SECS`,
//! `--allow-shared-filesystem`/`DJBOD_ALLOW_SHARED_FILESYSTEM`, and
//! `--tls-cert`, `--tls-key`, `--tls-ca`/`DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`,
//! `DJBOD_TLS_CA`. The file itself is `--config`/`DJBOD_CONFIG` and may be
//! omitted when the required settings come from elsewhere.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::transport::TlsPaths;

/// The default TCP port. It spells JBOD on a telephone keypad (J=5, B=2,
/// O=6, D=3) and IANA lists it as unassigned. The first candidate, 7400,
/// is the DDS/RTPS discovery port used by ROS 2 and was dropped for that
/// reason (SPEC 6.1.1).
pub const DEFAULT_PORT: u16 = 5263;

/// Temporary files older than this are deleted at startup (SPEC 10.11).
pub const DEFAULT_TEMPORARY_MAX_AGE_SECS: u64 = 3600;

/// A body or shard stream that delivers no frame for this long is
/// abandoned by its receiver (SPEC 10.12).
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfig {
    pub node_id: Uuid,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The address recorded in the cluster document for this node. Set it
    /// when `listen` is a wildcard address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise: Option<SocketAddr>,
    pub state_dir: PathBuf,
    pub devices: Vec<PathBuf>,
    #[serde(default)]
    pub bootstrap_peers: Vec<String>,
    #[serde(default = "default_temporary_max_age_secs")]
    pub temporary_max_age_secs: u64,
    /// Seconds a receiver waits for the next frame of a body or shard
    /// stream before abandoning the write (SPEC 10.12).
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    /// Permit two configured devices on one filesystem. This defeats the
    /// redundancy guarantee (SPEC 5.3) and exists only so that a node can
    /// be tried, or tested, with several directories on one disk. The
    /// node logs a warning at startup when it is set.
    #[serde(default)]
    pub allow_shared_filesystem: bool,
    /// Where this node's TLS material lives (SPEC 19.1.6.2). Required
    /// when the cluster's transport is not `plain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsPaths>,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))
}

fn default_temporary_max_age_secs() -> u64 {
    DEFAULT_TEMPORARY_MAX_AGE_SECS
}

fn default_stream_idle_timeout_secs() -> u64 {
    DEFAULT_STREAM_IDLE_TIMEOUT_SECS
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid node configuration: {reason}")]
    Parse { path: PathBuf, reason: String },
    #[error("configuration lists no devices")]
    NoDevices,
    #[error("device path {0} is listed more than once")]
    DuplicateDevice(PathBuf),
}

impl NodeConfig {
    pub fn load(path: &Path) -> Result<NodeConfig, ConfigError> {
        let config = NodeConfig::read(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse the file without validating it, for a caller that will
    /// overlay arguments and environment variables first (SPEC 20.6).
    pub fn read(path: &Path) -> Result<NodeConfig, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.devices.is_empty() {
            return Err(ConfigError::NoDevices);
        }
        for (i, device) in self.devices.iter().enumerate() {
            if self.devices[..i].contains(device) {
                return Err(ConfigError::DuplicateDevice(device.clone()));
            }
        }
        Ok(())
    }

    /// The address other nodes, and this node itself, connect to.
    /// The listen address when none is given: every interface, port 5263.
    pub fn default_listen() -> SocketAddr {
        default_listen()
    }

    pub fn advertised_address(&self) -> SocketAddr {
        self.advertise.unwrap_or(self.listen)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("a node configuration always serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_defaults() {
        let text = r#"
node_id = "2f1c6c3e-0000-4000-8000-000000000001"
state_dir = "/var/lib/djbod"
devices = ["/mnt/disk0/data", "/mnt/disk1/data"]
"#;
        let config: NodeConfig = toml::from_str(text).expect("parse");
        config.validate().expect("valid");
        assert_eq!(config.listen.port(), DEFAULT_PORT);
        assert!(config.bootstrap_peers.is_empty());
        assert_eq!(config.temporary_max_age_secs, 3600);
        assert!(!config.allow_shared_filesystem);
        assert_eq!(config.devices.len(), 2);
    }

    #[test]
    fn rejects_no_devices_and_duplicates() {
        let text = r#"
node_id = "2f1c6c3e-0000-4000-8000-000000000001"
state_dir = "/var/lib/djbod"
devices = []
"#;
        let config: NodeConfig = toml::from_str(text).expect("parse");
        assert!(matches!(config.validate(), Err(ConfigError::NoDevices)));

        let text = r#"
node_id = "2f1c6c3e-0000-4000-8000-000000000001"
state_dir = "/var/lib/djbod"
devices = ["/mnt/a", "/mnt/a"]
"#;
        let config: NodeConfig = toml::from_str(text).expect("parse");
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateDevice(_))
        ));
    }

    #[test]
    fn round_trips_through_toml() {
        let config = NodeConfig {
            node_id: Uuid::from_u128(7),
            listen: "127.0.0.1:5263".parse().expect("addr"),
            advertise: None,
            state_dir: PathBuf::from("/tmp/state"),
            devices: vec![PathBuf::from("/mnt/a")],
            bootstrap_peers: vec!["10.0.0.2:5263".to_string()],
            temporary_max_age_secs: 60,
            stream_idle_timeout_secs: 120,
            allow_shared_filesystem: false,
            tls: None,
        };
        let text = config.to_toml();
        let parsed: NodeConfig = toml::from_str(&text).expect("parse");
        assert_eq!(parsed, config);
    }
}
