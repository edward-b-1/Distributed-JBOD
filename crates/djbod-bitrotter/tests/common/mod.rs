use anyhow::{bail, Result};
use djbod_bitrotter::{
    config::{CoordinatorConfig, WorkerAddress, WorkerConfig},
    coordinator::{create_plan, Selection},
    model::{Consent, Plan, Reply, Request},
    network::{certificate_fingerprint, RpcClient},
};
use djbod_client::{transport::TlsPaths, Client, ClientOptions};
use djbod_core::{
    erasure::ShardIndex,
    layout::{object_directory, shard_file_name},
    record::{DeviceId, MetadataRecord},
};
use djbod_node::{
    config::NodeConfig,
    membership,
    node::{ClusterParameters, Node},
    server,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

pub const BLOCK: u64 = 64 * 1024;
pub const BINARY: &str = env!("CARGO_BIN_EXE_djbod-bitrotter");
pub const WORKER_BINARY: &str = env!("CARGO_BIN_EXE_djbod-bitrotter-worker");

struct ProductNode {
    config: NodeConfig,
    node: Arc<Node>,
    task: JoinHandle<()>,
}

impl Drop for ProductNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct WorkerProcess {
    pub config: WorkerConfig,
    pub endpoint: WorkerAddress,
    config_path: PathBuf,
    log_path: PathBuf,
    process: Option<Child>,
}

impl WorkerProcess {
    fn start(&mut self) -> Result<()> {
        let log = fs::File::create(&self.log_path)?;
        self.process = Some(
            Command::new(WORKER_BINARY)
                .arg("--config")
                .arg(&self.config_path)
                .env("TOKIO_WORKER_THREADS", "2")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()?,
        );
        Ok(())
    }
    pub fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    pub async fn restart(&mut self, rpc: &RpcClient) -> Result<()> {
        self.stop();
        self.start()?;
        self.ready(rpc).await
    }
    async fn ready(&mut self, rpc: &RpcClient) -> Result<()> {
        for _ in 0..200 {
            if matches!(
                rpc.request(&self.endpoint, Request::Describe).await,
                Ok(Reply::Description(_))
            ) {
                return Ok(());
            }
            if self.process.as_mut().unwrap().try_wait()?.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bail!(
            "worker failed to start: {}",
            fs::read_to_string(&self.log_path)?
        );
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Cluster {
    pub workers: Vec<WorkerProcess>,
    nodes: Vec<ProductNode>,
    pub config: CoordinatorConfig,
    pub rogue_tls: TlsPaths,
    pub roots: BTreeMap<DeviceId, PathBuf>,
    pub bootstrap: SocketAddr,
    pub dir: tempfile::TempDir,
}

impl Cluster {
    pub async fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate()?;
        let ca = ca_params.self_signed(&ca_key)?;
        let ca_path = dir.path().join("ca.pem");
        fs::write(&ca_path, ca.pem())?;
        let identity = |name: &str| -> Result<TlsPaths> {
            let key = KeyPair::generate()?;
            let cert = CertificateParams::new(vec!["127.0.0.1".to_string()])?
                .signed_by(&key, &ca, &ca_key)?;
            let cert_path = dir.path().join(format!("{name}.pem"));
            let key_path = dir.path().join(format!("{name}.key"));
            fs::write(&cert_path, cert.pem())?;
            fs::write(&key_path, key.serialize_pem())?;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
            Ok(TlsPaths {
                ca: ca_path.clone(),
                cert: cert_path,
                key: key_path,
            })
        };
        let tls = identity("controller")?;
        let rogue_tls = identity("unlisted-controller")?;
        let fingerprint = certificate_fingerprint(&tls.cert)?;
        let rpc = RpcClient::new(&tls)?;
        let mut nodes: Vec<ProductNode> = Vec::new();
        let mut workers = Vec::new();
        let mut roots = BTreeMap::new();
        for i in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let devices: Vec<_> = (0..2)
                .map(|d| dir.path().join(format!("node{i}-disk{d}")))
                .collect();
            for device in &devices {
                fs::create_dir(device)?;
            }
            let state_dir = dir.path().join(format!("node{i}-state"));
            fs::create_dir(&state_dir)?;
            let config = NodeConfig {
                node_id: Uuid::new_v4(),
                listen: address,
                advertise: None,
                state_dir,
                devices,
                bootstrap_peers: nodes
                    .last()
                    .map(|n| vec![n.config.listen.to_string()])
                    .unwrap_or_default(),
                temporary_max_age_secs: 3600,
                stream_idle_timeout_secs: 120,
                allow_shared_filesystem: true,
                tls: None,
            };
            let node = if let Some(peer) = nodes.last() {
                membership::join(&config, peer.config.listen, peer.node.cluster_id(), false)
                    .await?;
                Arc::new(Node::open(config.clone())?)
            } else {
                Arc::new(Node::init_cluster(
                    config.clone(),
                    ClusterParameters {
                        k: 3,
                        m: 2,
                        block_size: BLOCK,
                        headroom: 0.0,
                        ..ClusterParameters::default()
                    },
                )?)
            };
            for device in node.devices() {
                roots.insert(device.id(), device.root().to_path_buf());
            }
            let task = tokio::spawn(server::serve(node.clone(), listener));
            let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
            let worker_address = reservation.local_addr()?;
            let worker_config = WorkerConfig {
                node: node.id(),
                cluster: node.cluster_id(),
                listen: worker_address,
                devices: config.devices.clone(),
                journal: dir.path().join(format!("worker{i}.jsonl")),
                tls: identity(&format!("worker{i}"))?,
                allowed_controllers: vec![fingerprint.clone()],
            };
            let config_path = dir.path().join(format!("worker{i}.toml"));
            fs::write(&config_path, toml::to_string(&worker_config)?)?;
            let mut worker = WorkerProcess {
                config: worker_config,
                endpoint: WorkerAddress {
                    node: node.id(),
                    address: worker_address.to_string(),
                    server_name: None,
                },
                config_path,
                log_path: dir.path().join(format!("worker{i}.log")),
                process: None,
            };
            drop(reservation);
            worker.start()?;
            worker.ready(&rpc).await?;
            workers.push(worker);
            nodes.push(ProductNode { config, node, task });
        }
        Ok(Self {
            bootstrap: nodes[0].config.listen,
            config: CoordinatorConfig {
                tls,
                product_tls: None,
                workers: workers.iter().map(|w| w.endpoint.clone()).collect(),
            },
            rogue_tls,
            workers,
            nodes,
            roots,
            dir,
        })
    }
    pub async fn client(&self) -> Result<Client> {
        Ok(Client::connect(ClientOptions::new(vec![self.bootstrap])).await?)
    }
    pub async fn put(&self, key: &str, body: &[u8]) -> Result<MetadataRecord> {
        let mut client = self.client().await?;
        client.put(key, body, None).await?;
        Ok(client.head(key).await?.record)
    }
    pub async fn plan(&self, key: &str, n: u16, indices: Vec<u8>) -> Result<Plan> {
        create_plan(
            vec![self.bootstrap],
            self.config.clone(),
            Selection {
                n,
                seed: 42,
                keys: vec![key.to_string()],
                shard_indices: indices,
                ..Selection::default()
            },
        )
        .await
    }
    pub fn different_nodes(&self, record: &MetadataRecord) -> Vec<u8> {
        let document = self.nodes[0].node.document();
        let first = record.device_for(ShardIndex(0)).unwrap();
        let owner = document.device(first).unwrap().node;
        let second = record
            .shards
            .iter()
            .find(|s| document.device(s.device).unwrap().node != owner)
            .unwrap()
            .index;
        vec![0, second]
    }
    pub fn shard(&self, record: &MetadataRecord, index: u8) -> PathBuf {
        let root = &self.roots[&record.device_for(ShardIndex(index)).unwrap()];
        object_directory(root, &record.bucket, &record.key_hash)
            .join(shard_file_name(&record.version, ShardIndex(index)))
    }
    pub fn snapshot(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        fn visit(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
            for item in fs::read_dir(dir)? {
                let item = item?;
                if item.file_type()?.is_dir() {
                    visit(&item.path(), out)?;
                } else {
                    out.insert(item.path(), fs::read(item.path())?);
                }
            }
            Ok(())
        }
        let mut files = BTreeMap::new();
        for root in self.roots.values() {
            visit(root, &mut files)?;
        }
        Ok(files)
    }
}

pub fn consent(plan: &Plan) -> Consent {
    Consent {
        plan_id: plan.id.clone(),
        test_damage: true,
        data_loss: plan.destructive(),
    }
}

pub fn body(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| (i.wrapping_mul(73) ^ (i >> 8)) as u8)
        .collect()
}

pub fn cli() -> Command {
    let mut command = Command::new(BINARY);
    command
        .env("TOKIO_WORKER_THREADS", "2")
        .stdin(Stdio::null());
    command
}
