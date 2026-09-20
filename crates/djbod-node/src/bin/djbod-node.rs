//! The node binary: `init-cluster` creates a cluster from this node;
//! `run` serves it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing_subscriber::field::MakeExt as _;

use djbod_node::config::NodeConfig;
use djbod_node::node::{ClusterParameters, Node};
use djbod_node::server;

#[derive(Parser)]
#[command(name = "djbod-node", about = "Distributed-JBOD node", version = djbod_client::BUILD)]
struct Cli {
    /// Log output format (SPEC 20.4.3).
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LogFormat {
    /// Human-readable lines on standard error.
    Text,
    /// One JSON object per line, for a log collector.
    Json,
}

/// Where this node's TLS material lives (SPEC 19.1.6.2): flags win over
/// environment variables, which win over the `[tls]` table.
#[derive(clap::Args)]
struct TlsArgs {
    /// This node's PEM certificate (with an IP SAN for its address).
    #[arg(long, env = "DJBOD_TLS_CERT", requires_all = ["tls_key", "tls_ca"])]
    tls_cert: Option<PathBuf>,
    /// This node's PEM private key, readable only by its owner.
    #[arg(long, env = "DJBOD_TLS_KEY", requires_all = ["tls_cert", "tls_ca"])]
    tls_key: Option<PathBuf>,
    /// The cluster's PEM certificate authority, or a bundle.
    #[arg(long, env = "DJBOD_TLS_CA", requires_all = ["tls_cert", "tls_key"])]
    tls_ca: Option<PathBuf>,
}

impl TlsArgs {
    fn apply(&self, config: &mut NodeConfig) {
        if let (Some(cert), Some(key), Some(ca)) = (&self.tls_cert, &self.tls_key, &self.tls_ca) {
            config.tls = Some(djbod_node::transport::TlsPaths {
                cert: cert.clone(),
                key: key.clone(),
                ca: ca.clone(),
            });
        }
    }
}

/// The node's settings from three sources (SPEC 20.6): an argument wins
/// over an environment variable, which wins over the configuration file.
/// The file may be omitted when `node_id`, `state_dir`, and at least one
/// device come from the other two.
#[derive(clap::Args)]
struct ConfigArgs {
    /// The node configuration file (TOML).
    #[arg(long, env = "DJBOD_CONFIG")]
    config: Option<PathBuf>,
    /// This node's UUID.
    #[arg(long, env = "DJBOD_NODE_ID")]
    node_id: Option<uuid::Uuid>,
    /// Address to listen on.
    #[arg(long, env = "DJBOD_LISTEN")]
    listen: Option<std::net::SocketAddr>,
    /// Address other nodes use to reach this one, when `listen` is a
    /// wildcard. A change is proposed to the cluster when the node starts.
    #[arg(long, env = "DJBOD_ADVERTISE")]
    advertise: Option<std::net::SocketAddr>,
    /// Directory holding this node's copy of the cluster document.
    #[arg(long, env = "DJBOD_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// A device path; repeat for several. Replaces the file's list.
    #[arg(long = "device", env = "DJBOD_DEVICES", value_delimiter = ',')]
    devices: Vec<PathBuf>,
    /// A peer to consult at startup; repeat for several. Replaces the
    /// file's list.
    #[arg(
        long = "bootstrap-peer",
        env = "DJBOD_BOOTSTRAP_PEERS",
        value_delimiter = ','
    )]
    bootstrap_peers: Vec<String>,
    /// Temporary files older than this are deleted at startup.
    #[arg(long, env = "DJBOD_TEMPORARY_MAX_AGE_SECS")]
    temporary_max_age_secs: Option<u64>,
    /// Seconds to wait for the next frame of an upload before abandoning
    /// it.
    #[arg(long, env = "DJBOD_STREAM_IDLE_TIMEOUT_SECS")]
    stream_idle_timeout_secs: Option<u64>,
    /// Permit two devices on one filesystem (tests and experiments only).
    #[arg(long, env = "DJBOD_ALLOW_SHARED_FILESYSTEM")]
    allow_shared_filesystem: bool,
    #[command(flatten)]
    tls: TlsArgs,
}

impl ConfigArgs {
    fn resolve(&self) -> anyhow::Result<NodeConfig> {
        let mut config = match &self.config {
            Some(path) => NodeConfig::read(path).context("loading node configuration")?,
            None => {
                let node_id = self
                    .node_id
                    .context("no configuration file: --node-id or DJBOD_NODE_ID is required")?;
                let state_dir = self
                    .state_dir
                    .clone()
                    .context("no configuration file: --state-dir or DJBOD_STATE_DIR is required")?;
                if self.devices.is_empty() {
                    anyhow::bail!("no configuration file: --device or DJBOD_DEVICES is required");
                }
                NodeConfig {
                    node_id,
                    listen: NodeConfig::default_listen(),
                    advertise: None,
                    state_dir,
                    devices: Vec::new(),
                    bootstrap_peers: Vec::new(),
                    temporary_max_age_secs: djbod_node::config::DEFAULT_TEMPORARY_MAX_AGE_SECS,
                    stream_idle_timeout_secs: djbod_node::config::DEFAULT_STREAM_IDLE_TIMEOUT_SECS,
                    allow_shared_filesystem: false,
                    tls: None,
                }
            }
        };
        if let Some(node_id) = self.node_id {
            config.node_id = node_id;
        }
        if let Some(listen) = self.listen {
            config.listen = listen;
        }
        if let Some(advertise) = self.advertise {
            config.advertise = Some(advertise);
        }
        if let Some(state_dir) = &self.state_dir {
            config.state_dir = state_dir.clone();
        }
        if !self.devices.is_empty() {
            config.devices = self.devices.clone();
        }
        if !self.bootstrap_peers.is_empty() {
            config.bootstrap_peers = self.bootstrap_peers.clone();
        }
        if let Some(secs) = self.temporary_max_age_secs {
            config.temporary_max_age_secs = secs;
        }
        if let Some(secs) = self.stream_idle_timeout_secs {
            config.stream_idle_timeout_secs = secs;
        }
        if self.allow_shared_filesystem {
            config.allow_shared_filesystem = true;
        }
        self.tls.apply(&mut config);
        config.validate().context("node configuration")?;
        Ok(config)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create a new cluster consisting of this node and its devices.
    InitCluster {
        #[command(flatten)]
        config: ConfigArgs,
        /// A name for the cluster, shown beside its id: 1 to 128
        /// characters, spaces allowed. `djbod cluster set-name` changes it.
        #[arg(long, env = "DJBOD_CLUSTER_NAME")]
        name: Option<String>,
        /// Data shards per stripe.
        #[arg(long, default_value_t = 3)]
        k: u8,
        /// Parity shards per stripe.
        #[arg(long, default_value_t = 1)]
        m: u8,
        /// Shard block size in bytes; a multiple of 4096.
        #[arg(long, default_value_t = 1 << 20)]
        block_size: u64,
        /// Fraction of each device kept free.
        #[arg(long, default_value_t = 0.05)]
        headroom: f64,
        /// Sanity limit on key length, in bytes.
        #[arg(long, default_value_t = djbod_core::cluster::DEFAULT_MAX_KEY_BYTES)]
        max_key_bytes: u64,
        /// Maximum object size, in bytes.
        #[arg(long, default_value_t = djbod_core::cluster::DEFAULT_MAX_OBJECT_BYTES)]
        max_object_bytes: u64,
        /// Limit on an object's user metadata, keys and values together,
        /// in bytes.
        #[arg(long, default_value_t = djbod_core::cluster::DEFAULT_MAX_USER_METADATA_BYTES)]
        max_user_metadata_bytes: u64,
        /// Erase devices that belonged to a cluster before. Destroys their
        /// data.
        #[arg(long)]
        wipe_removed_device: bool,
    },
    /// Join an existing cluster: fetch its document from a peer,
    /// initialise this node's devices, and add this node to the document.
    Join {
        #[command(flatten)]
        config: ConfigArgs,
        /// Address of any node already in the cluster.
        #[arg(long)]
        peer: std::net::SocketAddr,
        /// The cluster id, as printed by init-cluster.
        #[arg(long)]
        cluster: uuid::Uuid,
        /// Erase devices that were removed from the cluster before and
        /// add them as new. Destroys their data.
        #[arg(long)]
        wipe_removed_device: bool,
    },
    /// Initialise a device path listed in the configuration but not yet
    /// in the cluster document, and add it. Restart the node afterwards.
    AddDevice {
        #[command(flatten)]
        config: ConfigArgs,
        /// The device path(s) to add; must appear in the configuration.
        #[arg(long, required = true)]
        path: Vec<PathBuf>,
        /// Address of any running node; defaults to this node's own.
        #[arg(long)]
        peer: Option<std::net::SocketAddr>,
        /// Erase a device that was removed from the cluster before and
        /// add it as new. Destroys its data.
        #[arg(long)]
        wipe_removed_device: bool,
    },
    /// Run the node. A node whose configured address is not the one the
    /// cluster document lists proposes the change first, and does not
    /// start if that fails.
    Run {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Offline check of this machine's devices: verify every record and
    /// shard block against its checksum and report what is wrong. Reads
    /// the disks directly and needs no running node. The cluster-wide
    /// scrub, which sweeps every node and can repair, is the client's
    /// `djbod scrub` (milestone 3).
    /// Pass `--device` to limit the scrub to some of the configured
    /// devices.
    Scrub {
        #[command(flatten)]
        config: ConfigArgs,
        /// Cap on read rate in MiB per second. Default: unlimited.
        #[arg(long)]
        rate_mib: Option<u64>,
        /// One JSON object per finding on standard output.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Logs go to standard error (SPEC 20.4.3); tracing-subscriber's own
    // default is standard output. Colour and styling only when a person
    // is watching; a log file or the journal gets plain text.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    // The default field formatter italicises field names on a terminal.
    // This one writes `name=value` plainly, leaving the level colours and
    // the dimmed target to the event formatter.
    let plain_fields = tracing_subscriber::fmt::format::debug_fn(
        |writer: &mut tracing_subscriber::fmt::format::Writer<'_>,
         field: &tracing::field::Field,
         value: &dyn std::fmt::Debug| {
            if field.name() == "message" {
                write!(writer, "{value:?}")
            } else {
                write!(writer, "{}={value:?}", field.name())
            }
        },
    )
    .delimited(" ");
    match cli.log_format {
        LogFormat::Text => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .fmt_fields(plain_fields)
            .with_env_filter(filter)
            .init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_env_filter(filter)
            .init(),
    }
    // A panic leaves a line in the same stream as everything else
    // (SPEC 20.4.5).
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = %info, "panic");
    }));
    match cli.command {
        Command::InitCluster {
            config,
            name,
            k,
            m,
            block_size,
            headroom,
            max_key_bytes,
            max_object_bytes,
            max_user_metadata_bytes,
            wipe_removed_device,
        } => {
            let config = config.resolve()?;
            if wipe_removed_device {
                for path in &config.devices {
                    if djbod_core::device::Device::open(path, None).is_ok() {
                        djbod_core::device::Device::erase(path)
                            .with_context(|| format!("wiping {}", path.display()))?;
                        eprintln!(
                            "wiped {}: every shard and record it held is gone",
                            path.display()
                        );
                    }
                }
            }
            let node = Node::init_cluster(
                config,
                ClusterParameters {
                    name,
                    k,
                    m,
                    block_size,
                    headroom,
                    max_key_bytes,
                    max_object_bytes,
                    max_user_metadata_bytes,
                },
            )
            .context("initialising cluster")?;
            println!("cluster {} created", node.cluster_id());
            println!("node    {}", node.id().0);
            for device in node.devices() {
                println!("device  {}  {}", device.id().0, device.root().display());
            }
            Ok(())
        }
        Command::Join {
            config,
            peer,
            cluster,
            wipe_removed_device,
        } => {
            let config = config.resolve()?;
            let document =
                djbod_node::membership::join(&config, peer, cluster, wipe_removed_device)
                    .await
                    .context("joining cluster")?;
            println!(
                "joined cluster {} as node {}",
                document.cluster_id, config.node_id
            );
            println!("document version {}", document.version);
            for entry in document
                .devices
                .iter()
                .filter(|d| d.node.0 == config.node_id)
            {
                println!("device  {}", entry.id.0);
            }
            println!("start the node with: djbod-node run --config <the same file>");
            Ok(())
        }
        Command::AddDevice {
            config,
            path,
            peer,
            wipe_removed_device,
        } => {
            let config = config.resolve()?;
            for p in &path {
                if !config.devices.contains(p) {
                    anyhow::bail!(
                        "{} is not listed under devices in the configuration",
                        p.display()
                    );
                }
            }
            let document = Node::load_document_for(&config)
                .context("reading saved cluster document")?
                .context("no cluster document; this node has not joined a cluster")?;
            let peer = peer.unwrap_or_else(|| config.advertised_address());
            let document = djbod_node::membership::add_devices(
                &config,
                &path,
                peer,
                document.cluster_id,
                wipe_removed_device,
            )
            .await
            .context("adding devices")?;
            println!("document version {}", document.version);
            println!("restart the node to serve the new device(s)");
            Ok(())
        }
        Command::Scrub {
            config,
            rate_mib,
            json,
        } => scrub(&config.resolve()?, rate_mib, json).await,
        Command::Run { config } => {
            let config = config.resolve()?;
            let listen = config.listen;
            djbod_node::membership::adopt_from_peers(&config)
                .await
                .context("checking bootstrap peers")?;
            let node = Arc::new(Node::open(config).context("opening node")?);
            let listener = TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!(
                node = %node.id().0,
                cluster = %node.cluster_id(),
                cluster_name = node.document().name.as_deref().unwrap_or("-"),
                document_version = node.document_version(),
                %listen,
                devices = node.devices().len(),
                transport = %node.document().transport,
                tls_material = node.tls().is_some(),
                "node running"
            );
            server::serve(node.clone(), listener).await;
            if node.is_removed() {
                // Let the acknowledgement of the document that removed us
                // reach the proposer before the process ends.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                eprintln!(
                    "this node was removed from the cluster at document version {}; stopping. Its devices can be reused with `djbod-node join --wipe-removed-device`, which erases them.",
                    node.document_version()
                );
            }
            Ok(())
        }
    }
}

/// The `scrub` subcommand: the local scrub engine run offline over this
/// machine's devices.
async fn scrub(config: &NodeConfig, rate_mib: Option<u64>, json: bool) -> anyhow::Result<()> {
    use djbod_core::device::Device;
    use djbod_core::scrub::{scrub_device, Finding, ScrubOptions};

    let document_path = config
        .state_dir
        .join(djbod_node::node::CLUSTER_DOCUMENT_FILE);
    let document: djbod_core::cluster::ClusterDocument = serde_json::from_str(
        &std::fs::read_to_string(&document_path)
            .with_context(|| format!("reading {}", document_path.display()))?,
    )
    .context("parsing cluster document")?;

    let options = ScrubOptions {
        max_bytes_per_second: rate_mib.map(|m| m * 1024 * 1024),
        temporary_max_age: std::time::Duration::from_secs(config.temporary_max_age_secs),
    };
    let selected: Vec<&PathBuf> = config.devices.iter().collect();

    let mut all_findings: Vec<Finding> = Vec::new();
    let mut totals = (0u64, 0u64, 0u64, 0u64);
    for path in selected {
        let device = Device::open(path, Some(document.cluster_id))
            .with_context(|| format!("opening device {}", path.display()))?;
        let mut print = |finding: &Finding| {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(finding).expect("finding serializes")
                );
            } else {
                println!("{}", describe_finding(finding));
            }
        };
        let summary = tokio::task::block_in_place(|| scrub_device(&device, &options, &mut print))
            .with_context(|| format!("scrubbing {}", path.display()))?;
        totals.0 += summary.records_checked;
        totals.1 += summary.shards_checked;
        totals.2 += summary.blocks_checked;
        totals.3 += summary.bytes_read;
        if !json {
            eprintln!(
                "{}: {} records, {} shards, {} blocks, {} read, {} finding(s)",
                path.display(),
                summary.records_checked,
                summary.shards_checked,
                summary.blocks_checked,
                human_bytes(summary.bytes_read),
                summary.findings.len()
            );
        }
        all_findings.extend(summary.findings);
    }
    if !json {
        eprintln!(
            "total: {} records, {} shards, {} blocks, {} read, {} finding(s)",
            totals.0,
            totals.1,
            totals.2,
            human_bytes(totals.3),
            all_findings.len()
        );
    }

    if all_findings.is_empty() {
        Ok(())
    } else {
        std::process::exit(2)
    }
}

fn describe_finding(finding: &djbod_core::scrub::Finding) -> String {
    use djbod_core::scrub::Finding::*;
    match finding {
        RecordCorrupt { path, reason } => {
            format!("record corrupt      {}  {reason}", path.display())
        }
        ShardUnreadable { path, reason, .. } => {
            format!("shard unreadable    {}  {reason}", path.display())
        }
        ShardMisplaced { path, reason, .. } => {
            format!("shard misplaced     {}  {reason}", path.display())
        }
        ShardBlocksCorrupt { path, stripes, .. } => {
            format!(
                "shard blocks corrupt {}  stripes {stripes:?}",
                path.display()
            )
        }
        ShardWithoutRecord { path, .. } => format!("shard without record {}", path.display()),
        RecordWithoutShard {
            path, shard_index, ..
        } => {
            format!(
                "record without shard {}  shard {shard_index} missing",
                path.display()
            )
        }
        RecordNotForThisDevice { path, .. } => {
            format!("record not for this device {}", path.display())
        }
        StaleTemporary { path, age_secs } => {
            format!("stale temporary     {}  {age_secs}s old", path.display())
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
