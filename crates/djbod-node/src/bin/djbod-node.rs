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
    #[command(subcommand)]
    command: Command,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
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
