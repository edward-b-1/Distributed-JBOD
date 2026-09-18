//! The node binary: `init-cluster` creates a cluster from this node;
//! `run` serves it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

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
    match cli.log_format {
        LogFormat::Text => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
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
        Command::Run { config } => {
            let config = NodeConfig::load(&config).context("loading node configuration")?;
            let listen = config.listen;
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
