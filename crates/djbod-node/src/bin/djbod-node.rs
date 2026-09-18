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
