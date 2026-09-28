use anyhow::{bail, ensure, Context, Result};
use djbod_client::transport::{Connector, TlsError, TlsPaths};
use djbod_core::cluster::{NodeId, Transport};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerAddress {
    pub node: NodeId,
    pub address: String,
    #[serde(default)]
    pub server_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorConfig {
    #[serde(default)]
    pub tls: Option<ClientTls>,
    #[serde(default)]
    pub product_tls: Option<ClientTls>,
    pub workers: Vec<WorkerAddress>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    #[serde(rename = "node_id", alias = "node")]
    pub node: NodeId,
    pub cluster: Uuid,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub devices: Vec<PathBuf>,
    pub journal: PathBuf,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsPaths>,
    /// SHA-256 fingerprints of explicitly allowed controller leaf certificates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_controllers: Vec<String>,
}

pub fn default_listen() -> SocketAddr {
    ([0, 0, 0, 0], crate::WORKER_PORT).into()
}

pub fn resolve(base: &Path, path: &mut PathBuf) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

pub fn resolve_tls(base: &Path, paths: &mut TlsPaths) {
    resolve(base, &mut paths.ca);
    resolve(base, &mut paths.cert);
    resolve(base, &mut paths.key);
}

/// Client convention from SPEC 19.1.6.2: CA only for anonymous TLS,
/// or CA plus both certificate and key for mutual TLS. No table means TCP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientTls {
    // Preserve the original TlsPaths field order in saved plan hashes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
    pub ca: PathBuf,
}

impl From<TlsPaths> for ClientTls {
    fn from(paths: TlsPaths) -> Self {
        Self {
            cert: Some(paths.cert),
            key: Some(paths.key),
            ca: paths.ca,
        }
    }
}

impl ClientTls {
    pub fn validate(&self) -> Result<()> {
        if self.cert.is_some() != self.key.is_some() {
            return Err(TlsError::IncompleteClientIdentity.into());
        }
        Ok(())
    }
    pub fn resolve(&mut self, base: &Path) {
        resolve(base, &mut self.ca);
        if let Some(cert) = &mut self.cert {
            resolve(base, cert);
        }
        if let Some(key) = &mut self.key {
            resolve(base, key);
        }
    }
    pub fn connector(&self) -> Result<Connector> {
        Ok(Connector::from_client_options(
            Some(&self.ca),
            self.cert.as_deref(),
            self.key.as_deref(),
        )?)
    }
}

impl CoordinatorConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::read(path)?;
        config.validate()?;
        Ok(config)
    }
    /// Read before applying command-line/environment overrides.
    pub fn read(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path).context("coordinator config path")?;
        let mut config: Self = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let base = path.parent().context("config parent")?;
        if let Some(tls) = &mut config.tls {
            tls.resolve(base);
        }
        if let Some(tls) = config.product_tls.as_mut() {
            tls.resolve(base);
        }
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(tls) = &self.tls {
            tls.validate()?;
        }
        if let Some(tls) = &self.product_tls {
            tls.validate()?;
        }
        if self.workers.is_empty() {
            bail!("no workers configured");
        }
        let mut nodes = std::collections::BTreeSet::new();
        let mut addresses = std::collections::BTreeSet::new();
        for worker in &self.workers {
            if !nodes.insert(worker.node) || !addresses.insert(worker.address.clone()) {
                bail!("duplicate worker node or endpoint");
            }
            if worker.address.is_empty() {
                bail!("empty worker endpoint");
            }
        }
        Ok(())
    }
}

impl WorkerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::read(path)?;
        config.validate()?;
        Ok(config)
    }
    /// Read before applying command-line/environment overrides.
    pub fn read(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path).context("worker config path")?;
        let mut config: Self = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let base = path.parent().context("config parent")?;
        if let Some(tls) = &mut config.tls {
            resolve_tls(base, tls);
        }
        resolve(base, &mut config.journal);
        for root in &mut config.devices {
            resolve(base, root);
        }
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.devices.is_empty() {
            bail!("no allowed device roots");
        }
        ensure!(
            self.transport == Transport::Plain || self.tls.is_some(),
            "transport {} requires TLS material (--tls-cert, --tls-key, --tls-ca or [tls])",
            self.transport
        );
        ensure!(
            self.allowed_controllers.is_empty() || self.tls.is_some(),
            "controller certificate allowlist requires TLS material"
        );
        for value in &self.allowed_controllers {
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("controller fingerprint must be 64 SHA-256 hexadecimal digits");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_defaults_to_the_requested_worker_port() {
        assert_eq!(
            default_listen(),
            "0.0.0.0:6666".parse::<SocketAddr>().unwrap()
        );
        let json = serde_json::json!({
            "node": Uuid::new_v4(), "cluster": Uuid::new_v4(),
            "devices": ["/tmp/test-device"], "journal": "/tmp/journal",
            "allowed_controllers": ["ab".repeat(32)],
            "tls": { "ca": "ca.crt", "cert": "worker.crt", "key": "worker.key" }
        });
        let config: WorkerConfig = serde_json::from_value(json).unwrap();
        config.validate().unwrap();
        assert_eq!(config.listen.port(), 6666);
    }

    #[test]
    fn plaintext_needs_no_certificates_but_tls_modes_and_allowlists_do() {
        let json = serde_json::json!({
            "node_id": Uuid::new_v4(), "cluster": Uuid::new_v4(),
            "devices": ["/tmp/test-device"], "journal": "/tmp/journal"
        });
        let mut config: WorkerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.transport, Transport::Plain);
        config.validate().unwrap();
        for transport in [Transport::TlsOptional, Transport::Tls] {
            config.transport = transport;
            assert!(config.validate().is_err());
        }
        config.transport = Transport::Plain;
        config.allowed_controllers.push("ab".repeat(32));
        assert!(config.validate().is_err());
    }

    #[test]
    fn optional_client_identity_preserves_existing_plan_serialization() {
        let previous = TlsPaths {
            cert: "/tmp/controller.crt".into(),
            key: "/tmp/controller.key".into(),
            ca: "/tmp/ca.crt".into(),
        };
        let previous_json = serde_json::to_string(&previous).unwrap();
        let mut current: ClientTls = serde_json::from_str(&previous_json).unwrap();
        current.validate().unwrap();
        assert_eq!(serde_json::to_string(&current).unwrap(), previous_json);
        current.cert = None;
        assert!(current.validate().is_err());
        current.key = None;
        current.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::json!({"ca": "/tmp/ca.crt"})
        );
    }
}
