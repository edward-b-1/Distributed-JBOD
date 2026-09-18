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
#[command(name = "djbod-node", about = "Distributed-JBOD node")]
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

#[derive(Subcommand)]
enum Command {
    /// Create a new cluster consisting of this node and its devices.
    InitCluster {
        #[arg(long)]
        config: PathBuf,
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
    },
    /// Join an existing cluster: fetch its document from a peer,
    /// initialise this node's devices, and add this node to the document.
    Join {
        #[arg(long)]
        config: PathBuf,
        /// Address of any node already in the cluster.
        #[arg(long)]
        peer: std::net::SocketAddr,
        /// The cluster id, as printed by init-cluster.
        #[arg(long)]
        cluster: uuid::Uuid,
    },
    /// Initialise a device path listed in the configuration but not yet
    /// in the cluster document, and add it. Restart the node afterwards.
    AddDevice {
        #[arg(long)]
        config: PathBuf,
        /// The device path(s) to add; must appear in the configuration.
        #[arg(long, required = true)]
        path: Vec<PathBuf>,
        /// Address of any running node; defaults to this node's own.
        #[arg(long)]
        peer: Option<std::net::SocketAddr>,
    },
    /// Run the node.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Verify every record and shard block on this node's devices against
    /// their checksums and report what is wrong. Reads the disks directly;
    /// the node may be running or not. With --repair, also asks the
    /// running node to rebuild each damaged object.
    Scrub {
        #[arg(long)]
        config: PathBuf,
        /// Only this device path (may be repeated). Default: all.
        #[arg(long)]
        device: Vec<PathBuf>,
        /// Cap on read rate in MiB per second. Default: unlimited.
        #[arg(long)]
        rate_mib: Option<u64>,
        /// One JSON object per finding on standard output.
        #[arg(long)]
        json: bool,
        /// Repair each damaged object through the running node.
        #[arg(long)]
        repair: bool,
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
            k,
            m,
            block_size,
            headroom,
        } => {
            let config = NodeConfig::load(&config).context("loading node configuration")?;
            let node = Node::init_cluster(
                config,
                ClusterParameters {
                    k,
                    m,
                    block_size,
                    headroom,
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
        } => {
            let config = NodeConfig::load(&config).context("loading node configuration")?;
            let document = djbod_node::membership::join(&config, peer, cluster)
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
        Command::AddDevice { config, path, peer } => {
            let config = NodeConfig::load(&config).context("loading node configuration")?;
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
            let document =
                djbod_node::membership::add_devices(&config, &path, peer, document.cluster_id)
                    .await
                    .context("adding devices")?;
            println!("document version {}", document.version);
            println!("restart the node to serve the new device(s)");
            Ok(())
        }
        Command::Scrub {
            config,
            device,
            rate_mib,
            json,
            repair,
        } => scrub(&config, &device, rate_mib, json, repair).await,
        Command::Run { config } => {
            let config = NodeConfig::load(&config).context("loading node configuration")?;
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
                document_version = node.document_version(),
                %listen,
                devices = node.devices().len(),
                "node running"
            );
            server::serve(node, listener).await;
            Ok(())
        }
    }
}

/// The `scrub` subcommand.
async fn scrub(
    config_path: &std::path::Path,
    only: &[PathBuf],
    rate_mib: Option<u64>,
    json: bool,
    repair: bool,
) -> anyhow::Result<()> {
    use djbod_core::device::Device;
    use djbod_core::scrub::{scrub_device, Finding, ScrubOptions};
    use djbod_node::client::Connection;
    use djbod_proto::message::{Request, Response};

    let config = NodeConfig::load(config_path).context("loading node configuration")?;
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
    let selected: Vec<&PathBuf> = config
        .devices
        .iter()
        .filter(|p| only.is_empty() || only.contains(p))
        .collect();
    if selected.is_empty() {
        anyhow::bail!("no configured device matches --device");
    }

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

    if repair {
        let mut keys: Vec<&str> = all_findings.iter().filter_map(|f| f.repair_key()).collect();
        keys.sort_unstable();
        keys.dedup();
        if !keys.is_empty() {
            let address = config.advertised_address();
            let mut connection =
                Connection::connect(address, Connection::client_hello(document.cluster_id))
                    .await
                    .with_context(|| {
                        format!("connecting to the node at {address} for repair; is it running?")
                    })?;
            for key in keys {
                match connection
                    .request(Request::RepairObject {
                        key: key.to_string(),
                    })
                    .await
                {
                    Ok(Response::RepairObject(report)) => {
                        let rewritten = report.shards.iter().filter(|s| s.rewritten).count();
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string(&report).expect("report serializes")
                            );
                        } else {
                            eprintln!("repaired {key}: {rewritten} shard(s) rewritten");
                        }
                    }
                    Ok(other) => eprintln!("repair of {key}: unexpected response {other:?}"),
                    Err(e) => eprintln!("repair of {key} failed: {e}"),
                }
            }
        }
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
