use anyhow::{bail, Context, Result};
use djbod_client::transport::TlsPaths;
use djbod_core::cluster::NodeId;
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
    pub tls: TlsPaths,
    #[serde(default)]
    pub product_tls: Option<TlsPaths>,
    pub workers: Vec<WorkerAddress>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub node: NodeId,
    pub cluster: Uuid,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub devices: Vec<PathBuf>,
    pub journal: PathBuf,
    pub tls: TlsPaths,
    /// SHA-256 fingerprints of explicitly allowed controller leaf certificates.
    pub allowed_controllers: Vec<String>,
}

fn default_listen() -> SocketAddr {
    ([0, 0, 0, 0], crate::WORKER_PORT).into()
}

fn resolve(base: &Path, path: &mut PathBuf) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

fn resolve_tls(base: &Path, paths: &mut TlsPaths) {
    resolve(base, &mut paths.ca);
    resolve(base, &mut paths.cert);
    resolve(base, &mut paths.key);
}

impl CoordinatorConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path).context("coordinator config path")?;
        let mut config: Self = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let base = path.parent().context("config parent")?;
        resolve_tls(base, &mut config.tls);
        if let Some(tls) = config.product_tls.as_mut() {
            resolve_tls(base, tls);
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
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
        let path = std::fs::canonicalize(path).context("worker config path")?;
        let mut config: Self = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let base = path.parent().context("config parent")?;
        resolve_tls(base, &mut config.tls);
        resolve(base, &mut config.journal);
        for root in &mut config.devices {
            resolve(base, root);
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.devices.is_empty() {
            bail!("no allowed device roots");
        }
        if self.allowed_controllers.is_empty() {
            bail!("controller certificate allowlist is empty");
        }
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
}
