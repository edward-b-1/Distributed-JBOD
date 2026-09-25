//! `djbod`: the command-line client (SPEC C.2).
//!
//! ```text
//! djbod --node 10.0.0.1:5263 --cluster <uuid> status
//! djbod ... put photos/cat.jpg ./cat.jpg
//! djbod ... get photos/cat.jpg ./copy.jpg
//! djbod ... head photos/cat.jpg
//! djbod ... list --prefix photos/
//! djbod ... delete photos/cat.jpg
//! djbod ... cluster-config
//! ```
//!
//! `--node` and `--cluster` may also come from `DJBOD_NODE` and
//! `DJBOD_CLUSTER`. `init-cluster` prints the cluster id to use.
//!
//! Object bodies stream: `put` reads the file a chunk at a time, `get`
//! writes as chunks arrive. A failed `get` leaves a partial output file
//! and says so; it is removed when the output is a named file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use djbod_client::connection::ConnectionError;
use djbod_client::transport::Connector;
use djbod_client::{Client, ClientOptions};
use djbod_core::cluster::{DeviceState, NodeId};
use djbod_core::record::DeviceId;
use djbod_core::stripe::FaultKind;
use djbod_proto::message::{
    DrainEvent, ErrorCode, ErrorDetail, ListQuery, MissingRecordCopy, Reconstruction,
    RecordCopyFault,
};

mod tables;

#[derive(Parser)]
#[command(name = "djbod", about = "Distributed-JBOD client", version = djbod_client::BUILD)]
struct Cli {
    /// Address of a node in the cluster; several, comma-separated, are
    /// tried in order, and a request moves to the next when one fails.
    #[arg(
        long = "node",
        env = "DJBOD_NODE",
        global = true,
        value_delimiter = ','
    )]
    nodes: Vec<SocketAddr>,
    /// The cluster id, as printed by `djbod-node init-cluster`.
    #[arg(long, env = "DJBOD_CLUSTER", global = true)]
    cluster: Option<Uuid>,
    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// The cluster's PEM certificate authority (or bundle). Given alone,
    /// connections are encrypted but present no client certificate.
    #[arg(long, env = "DJBOD_TLS_CA", global = true)]
    tls_ca: Option<PathBuf>,
    /// This client's PEM certificate; needs --tls-key and --tls-ca.
    #[arg(long, env = "DJBOD_TLS_CERT", global = true, requires_all = ["tls_key", "tls_ca"])]
    tls_cert: Option<PathBuf>,
    /// This client's PEM private key, readable only by its owner.
    #[arg(long, env = "DJBOD_TLS_KEY", global = true, requires_all = ["tls_cert", "tls_ca"])]
    tls_key: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// The device a UUID or label names (SPEC 6.2.5.1), looked up in the
/// cluster document.
async fn resolve_device(cli: &Cli, name: &str) -> anyhow::Result<DeviceId> {
    let (node, cluster) = reachable_node(cli).await?;
    djbod_client::admin::resolve_device(&connector(cli)?, node, cluster, name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The node a UUID or label names (SPEC 6.2.5.1), looked up in the
/// cluster document.
async fn resolve_node(cli: &Cli, name: &str) -> anyhow::Result<NodeId> {
    let (node, cluster) = reachable_node(cli).await?;
    djbod_client::admin::resolve_node(&connector(cli)?, node, cluster, name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// How this client connects (SPEC 19.1.6.2), decided by the transport
/// module so that every client program decides it the same way.
fn connector(cli: &Cli) -> anyhow::Result<Connector> {
    Connector::from_client_options(
        cli.tls_ca.as_deref(),
        cli.tls_cert.as_deref(),
        cli.tls_key.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Subcommand)]
enum Command {
    /// Show every device's state and free space.
    Status,
    /// What each device holds: versions, keys, blocks and shard bytes,
    /// counted from its records without reading data. A device with zero
    /// versions is empty and may be removed.
    Contents {
        /// Devices to show, by UUID or label; every device when none.
        devices: Vec<String>,
        /// Only devices of this node, by UUID or label.
        #[arg(long)]
        node_id: Option<String>,
    },
    /// Ask a node which cluster it serves. Needs --node only; prints the
    /// cluster id alone, for `export DJBOD_CLUSTER=$(djbod get-cluster-id ...)`.
    /// --json adds the name and the node's build.
    GetClusterId,
    /// Who is at --node: the cluster's name and id, the node's label, id
    /// and address, its build, the document version it holds, and the
    /// transport. Needs --node only.
    Identity,
    /// Store a file (or standard input with `-`) under a key.
    Put {
        key: String,
        /// File to upload, or `-` for standard input (which is read fully
        /// first, since the size must be known).
        file: PathBuf,
        #[arg(long)]
        content_type: Option<String>,
    },
    /// Fetch an object to a file (or standard output with `-`, the default).
    Get {
        key: String,
        #[arg(default_value = "-")]
        file: PathBuf,
    },
    /// Show an object's metadata record.
    Head { key: String },
    /// Delete an object.
    Delete { key: String },
    /// List keys.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        start_after: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Print the cluster document.
    ClusterConfig,
    /// Cluster membership commands.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Rebuild damaged or missing shards of an object from the intact ones.
    Repair { key: String },
    /// Move one shard of an object to another device.
    MoveShard {
        key: String,
        shard_index: u8,
        /// Destination device, by UUID or label; chosen like a write if
        /// omitted.
        #[arg(long)]
        to: Option<String>,
    },
    /// Scrub the whole cluster: every node checks its own disks, then the
    /// cross-node checks run; with --repair, damaged objects are rebuilt.
    Scrub {
        /// Cap on each node's read rate in MiB per second.
        #[arg(long)]
        rate_mib: Option<u64>,
        #[arg(long)]
        repair: bool,
    },
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Ask every node for its document and show who holds which version.
    Show,
    /// Bring every node up to the highest document version any holds.
    Sync,
    /// Mark a device draining (it receives no new shards) or active again.
    /// Moves no data. The device is named by UUID or label.
    SetState { device: String, state: StateArg },
    /// Give a device a short name shown beside its UUID, or clear it with
    /// --clear. Labels are unique within the cluster.
    SetLabel {
        /// The device, by UUID or current label.
        device: String,
        /// The new label: 1 to 128 characters, no whitespace.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        label: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Give the cluster a name shown beside its id, or clear it with
    /// --clear. The id stays what `--cluster` takes.
    SetName {
        /// The new name: 1 to 128 characters; spaces are allowed, so quote
        /// it.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        name: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Give a node a short name shown beside its UUID, or clear it with
    /// --clear. Node labels are unique within the cluster.
    SetNodeLabel {
        /// The node, by UUID or current label.
        node_id: String,
        /// The new label: 1 to 128 characters, no whitespace.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        label: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Replace the addresses other nodes and clients use to reach a
    /// node. The node must still answer at its listed address; a node
    /// that has already moved adopts its configured address when it
    /// starts (SPEC 18.1.2.1).
    SetAddress {
        /// The node, by UUID or label.
        node_id: String,
        /// One or more ip:port addresses, comma separated; the first is
        /// the one used.
        #[arg(value_delimiter = ',', required = true)]
        addresses: Vec<String>,
    },
    /// Move every shard off a draining device in one pass.
    Drain {
        /// The draining device to empty, by UUID or label.
        #[arg(required_unless_present = "node_id", conflicts_with = "node_id")]
        device: Option<String>,
        /// Drain every draining device of this node in turn, by UUID or
        /// label.
        #[arg(long = "node-id", value_name = "NODE")]
        node_id: Option<String>,
        /// Start even if the estimate says not everything will fit.
        #[arg(long)]
        partial: bool,
    },
    /// Change the global k and m (and optionally the block size). Moves no
    /// data: new writes use the new scheme, existing objects keep theirs
    /// until `reencode` is run.
    SetScheme {
        /// Data shards per stripe.
        #[arg(long)]
        k: u8,
        /// Parity shards per stripe.
        #[arg(long)]
        m: u8,
        /// Shard block size in bytes; a multiple of 4096. Unchanged if
        /// omitted.
        #[arg(long)]
        block_size: Option<u64>,
    },
    /// Rewrite every object still at a scheme or block size other than
    /// the document's, one at a time. Safe to interrupt and rerun.
    Reencode,
    /// Change how connections are made: plain, tls-optional, or tls
    /// (SPEC 19.1.6.4). Moving off plain needs every node to have TLS
    /// material loaded.
    SetTransport { transport: DeviceTransport },
    /// Change the key length, object size, or user metadata limit.
    /// Applies to new writes; existing objects are untouched.
    SetLimits {
        /// Sanity limit on key length, in bytes.
        #[arg(long, required_unless_present_any = ["max_object_bytes", "max_user_metadata_bytes"])]
        max_key_bytes: Option<u64>,
        /// Maximum object size, in bytes.
        #[arg(long, required_unless_present_any = ["max_key_bytes", "max_user_metadata_bytes"])]
        max_object_bytes: Option<u64>,
        /// Limit on an object's user metadata, keys and values together,
        /// in bytes.
        #[arg(long, required_unless_present_any = ["max_key_bytes", "max_object_bytes"])]
        max_user_metadata_bytes: Option<u64>,
    },
    /// Mark a device removed. Refused while any object still has a shard
    /// on it: drain it first. The device is named by UUID or label.
    RemoveDevice {
        device: String,
        /// The device is dead or gone and cannot be drained: mark it
        /// removed anyway, without checking what it holds. Its shards are
        /// rebuilt elsewhere by `djbod scrub --repair`; a version with more
        /// than m shards on it is lost. Asks for confirmation first.
        #[arg(long)]
        force: bool,
        /// Skip the confirmation prompt of --force.
        #[arg(long, requires = "force")]
        yes: bool,
    },
    /// Drop a node and its devices from the cluster. Refused while any
    /// object still has a shard on them: drain them first. The node stops
    /// serving once it has acknowledged.
    RemoveNode {
        /// The node, by UUID or label.
        node_id: String,
        /// The node is permanently gone and cannot acknowledge: remove it
        /// without its agreement and rebuild its shards from parity.
        /// Shows the cost and asks for confirmation first.
        #[arg(long)]
        force: bool,
        /// Skip the confirmation prompt of --force.
        #[arg(long, requires = "force")]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum StateArg {
    Draining,
    Active,
}

#[derive(Clone, Copy, ValueEnum)]
enum DeviceTransport {
    Plain,
    TlsOptional,
    Tls,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The client library's client (SPEC 20.8): the configured nodes, tried
/// in order, and the cluster id, which every command but `identity` and
/// `get-cluster-id` requires.
async fn connect(cli: &Cli) -> anyhow::Result<Client> {
    if cli.nodes.is_empty() {
        bail!("no node address: pass --node or set DJBOD_NODE");
    }
    let cluster = cli
        .cluster
        .context("no cluster id: pass --cluster or set DJBOD_CLUSTER")?;
    connect_with_cluster(cli, Some(cluster)).await
}

async fn connect_with_cluster(cli: &Cli, cluster: Option<Uuid>) -> anyhow::Result<Client> {
    if cli.nodes.is_empty() {
        bail!("no node address: pass --node or set DJBOD_NODE");
    }
    let mut options = ClientOptions::new(cli.nodes.clone()).connector(connector(cli)?);
    options.cluster = cluster;
    Client::connect(options).await.map_err(client_err)
}

/// A node that answers, and the cluster id, for the membership
/// procedures, which take one peer address.
async fn reachable_node(cli: &Cli) -> anyhow::Result<(SocketAddr, Uuid)> {
    let client = connect(cli).await?;
    let node = client.node_address().context("not connected")?;
    Ok((node, client.cluster_id()))
}

/// Ask the configured nodes, in order, who they are (SPEC 19.1.5.1).
async fn ask_any_node(cli: &Cli) -> anyhow::Result<djbod_client::Identity> {
    if cli.nodes.is_empty() {
        bail!("no node address: pass --node or set DJBOD_NODE");
    }
    let connector = connector(cli)?;
    let mut attempts = Vec::new();
    for &address in &cli.nodes {
        match djbod_client::ask(&connector, address).await {
            Ok(identity) => return Ok(identity),
            Err(e) => attempts.push(format!("{address}: {}", client_err(e))),
        }
    }
    bail!("no node answered: {}", attempts.join("; "))
}

/// The library's error as an administrator wants to read it (SPEC 16.2).
fn client_err(e: djbod_client::ClientError) -> anyhow::Error {
    match e {
        djbod_client::ClientError::Connection(inner) => remote(*inner),
        other => anyhow::anyhow!("{other}"),
    }
}

/// Render a client error the way an administrator wants to read it: the
/// code, the message, then every identifying field the node supplied
/// (SPEC 16.2).
fn describe_error(e: &ConnectionError) -> String {
    match e {
        ConnectionError::Remote(detail) | ConnectionError::StreamFailed(detail) => {
            describe_detail(detail)
        }
        other => other.to_string(),
    }
}

fn describe_detail(detail: &ErrorDetail) -> String {
    let mut out = format!("{:?}: {}", detail.code, detail.message);
    if let Some(node) = detail.node {
        out.push_str(&format!("\n  node:    {}", node.0));
    }
    if let Some(device) = detail.device {
        out.push_str(&format!("\n  device:  {}", device.0));
    }
    if let Some(key) = &detail.key {
        out.push_str(&format!("\n  key:     {key}"));
    }
    if let Some(version) = detail.version {
        out.push_str(&format!("\n  version: {version}"));
    }
    if let Some(index) = detail.shard_index {
        out.push_str(&format!("\n  shard:   {index}"));
    }
    if let Some(stripe) = detail.stripe {
        out.push_str(&format!("\n  stripe:  {stripe}"));
    }
    out
}

/// The build of the node that answered, and this client's when it differs:
/// a mismatch between the two is the first thing worth noticing (SPEC
/// 6.2.6.4).
fn build_text(node: &str, client: &str) -> String {
    if node == client {
        node.to_string()
    } else {
        format!("{node} (this client: {client})")
    }
}

/// The exit code of `scrub` (SPEC 20.1.2.3): 0 when the run completed
/// and nothing remains wrong; 2 when it completed and damage remains; 3
/// when it did not complete and no damage was seen; 4 when it did not
/// complete and damage was seen. Damage remaining is the findings, or
/// with --repair the repairs that failed.
fn scrub_exit_code(repair: bool, incomplete: bool, findings: usize, repair_failures: usize) -> i32 {
    let damage_remaining = if repair {
        repair_failures > 0
    } else {
        findings > 0
    };
    match (incomplete, findings > 0) {
        (false, _) if !damage_remaining => 0,
        (false, _) => 2,
        (true, false) => 3,
        (true, true) => 4,
    }
}

/// The last line's verdict, in the words of SPEC 20.1.2.3.
fn scrub_outcome(
    repair: bool,
    incomplete: bool,
    findings: usize,
    repair_failures: usize,
) -> &'static str {
    match scrub_exit_code(repair, incomplete, findings, repair_failures) {
        0 if repair => "complete, everything found was repaired",
        0 => "complete, no damage found",
        2 if repair => "complete, some damage could not be repaired",
        2 => "complete, damage found",
        3 => "incomplete, no damage seen; run it again",
        _ if repair => "incomplete, damage found was repaired where it could be; whether more exists is unknown, run it again",
        _ => "incomplete, damage found and more may exist; run it again",
    }
}

fn remote(e: ConnectionError) -> anyhow::Error {
    anyhow::anyhow!("{}", describe_error(&e))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::GetClusterId => {
            let identity = ask_any_node(&cli).await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "cluster_id": identity.cluster_id,
                        "cluster_name": identity.cluster_name,
                        "node": identity.node,
                        "build": identity.build,
                    }))?
                );
            } else {
                println!("{}", identity.cluster_id);
            }
        }
        Command::Identity => {
            // Ask first (SPEC 19.1.5.1), then connect properly with the
            // answer for what only the document knows: label and address.
            let hello = ask_any_node(&cli).await?;
            let mut client = connect_with_cluster(&cli, Some(hello.cluster_id)).await?;
            let document = client.cluster_document().await.map_err(client_err)?;
            let entry = hello.node.and_then(|id| document.node(id).cloned());
            let label = entry.as_ref().and_then(|n| n.label.clone());
            let addresses: Vec<String> = entry.map(|n| n.addresses).unwrap_or_default();
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "cluster_id": hello.cluster_id,
                        "cluster_name": document.name,
                        "node": hello.node,
                        "node_label": label,
                        "addresses": addresses,
                        "build": hello.build,
                        "document_version": document.version,
                        "transport": document.transport.to_string(),
                    }))?
                );
            } else {
                println!("cluster   {}", document.title());
                let node_text = match (&label, hello.node) {
                    (Some(label), Some(id)) => format!("{label} ({})", id.0),
                    (None, Some(id)) => id.0.to_string(),
                    (_, None) => "-".to_string(),
                };
                println!("node      {node_text} at {}", addresses.join(", "));
                println!("build     {}", hello.build);
                println!("document  version {}", document.version);
                println!("transport {}", document.transport);
            }
        }
        Command::Contents { devices, node_id } => {
            let mut client = connect(&cli).await?;
            let document = client.cluster_document().await.map_err(client_err)?;
            let only_node = match node_id {
                Some(name) => Some(resolve_node(&cli, name).await?),
                None => None,
            };
            let mut chosen: Vec<DeviceId> = Vec::new();
            for name in devices {
                chosen.push(resolve_device(&cli, name).await?);
            }
            if chosen.is_empty() {
                chosen = document
                    .devices
                    .iter()
                    .filter(|d| only_node.is_none_or(|n| d.node == n))
                    .filter(|d| d.state != DeviceState::Removed)
                    .map(|d| d.id)
                    .collect();
            }
            let mut rows = Vec::with_capacity(chosen.len());
            for device in chosen {
                match client.device_contents(device).await {
                    Ok(contents) => rows.push(contents),
                    // A device its node cannot read (5.6) has no counts;
                    // say so and go on with the others.
                    Err(e)
                        if e.detail()
                            .is_some_and(|d| d.code == ErrorCode::DeviceUnavailable) =>
                    {
                        eprintln!(
                            "{device} unavailable: {}",
                            e.detail().map(|d| d.message.clone()).unwrap_or_default()
                        );
                    }
                    Err(e) => return Err(client_err(e)),
                }
            }
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", tables::contents(&document, &rows));
                let empty = rows.iter().filter(|c| c.versions == 0).count();
                if empty > 0 {
                    eprintln!("{empty} device(s) hold nothing and may be removed");
                }
            }
        }
        Command::Status => {
            let mut client = connect(&cli).await?;
            let djbod_client::Status {
                cluster_id,
                cluster_name,
                document_version,
                coordinator,
                nodes,
                transport,
                devices,
            } = client.status().await.map_err(client_err)?;
            // The build of the node that answered, from its Hello (SPEC
            // 19.1.5): the connection that served the request is still
            // the current one.
            let build = client.identity().await.map_err(client_err)?.build;
            {
                {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "cluster_id": cluster_id,
                                "cluster_name": cluster_name,
                                "document_version": document_version,
                                "coordinator": coordinator,
                                "build": build,
                                "nodes": nodes,
                                "transport": transport.to_string(),
                                "devices": devices,
                            }))?
                        );
                    } else {
                        match &cluster_name {
                            Some(name) => println!("cluster   {name} ({cluster_id})"),
                            None => println!("cluster   {cluster_id}"),
                        }
                        println!("document  version {document_version}");
                        println!("answered  by node {}", coordinator.0);
                        println!("build     {}", build_text(&build, djbod_client::BUILD));
                        println!("transport {transport}");
                        println!();
                        print!("{}", tables::status(&devices, &nodes));
                        let unavailable_count = devices
                            .iter()
                            .filter(|d| !d.available && d.state != DeviceState::Removed)
                            .count();
                        if unavailable_count > 0 {
                            eprintln!(
                                "{unavailable_count} device(s) unavailable: their node cannot read them (disk failed, not mounted, or destroyed) or cannot be reached"
                            );
                        }
                        // A node that could not be asked (5.6, 19.1.3): its
                        // devices are the unavailable ones above.
                        for n in nodes.iter().filter(|n| !n.reachable) {
                            eprintln!(
                                "node {} unreachable: {}",
                                n.node.0,
                                n.error.as_deref().unwrap_or("no answer")
                            );
                        }
                    }
                }
            }
        }
        Command::Put {
            key,
            file,
            content_type,
        } => {
            let mut client = connect(&cli).await?;
            let write = if file.as_os_str() == "-" {
                let mut body = Vec::new();
                tokio::io::stdin()
                    .read_to_end(&mut body)
                    .await
                    .context("reading standard input")?;
                client
                    .put(key, &body, content_type.clone())
                    .await
                    .map_err(client_err)?
            } else {
                let mut source = tokio::fs::File::open(file)
                    .await
                    .with_context(|| format!("opening {}", file.display()))?;
                let size = source.metadata().await?.len();
                client
                    .put_from_reader(
                        key,
                        size,
                        &mut source,
                        content_type.clone(),
                        Default::default(),
                    )
                    .await
                    .map_err(client_err)?
            };
            let version = write.version;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "key": key, "version": version.to_text(), "unavailable": write.unavailable })
                );
            } else {
                println!("stored {key} as version {version}");
            }
            // The write went around devices the cluster cannot read (SPEC
            // 5.6): the object is safe on the others, but say so, and exit
            // 2 so a pipeline notices.
            if !write.unavailable.is_empty() {
                let devices: Vec<String> = write
                    .unavailable
                    .iter()
                    .map(|u| format!("{} on node {}", u.device.0, u.node.0))
                    .collect();
                eprintln!(
                    "{key}: placed around {} unavailable device(s): {}; run `djbod status`",
                    devices.len(),
                    devices.join(", ")
                );
                std::process::exit(2);
            }
        }
        Command::Get { key, file } => {
            let mut client = connect(&cli).await?;
            let read = if file.as_os_str() == "-" {
                let mut stdout = tokio::io::stdout();
                let read = client
                    .get_to_writer(key, &mut stdout)
                    .await
                    .map_err(client_err)?;
                stdout.flush().await?;
                read
            } else {
                let mut sink = tokio::fs::File::create(file)
                    .await
                    .with_context(|| format!("creating {}", file.display()))?;
                let result = client.get_to_writer(key, &mut sink).await;
                match result {
                    Ok(read) => {
                        sink.sync_all().await?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&read.record)?);
                        } else {
                            eprintln!(
                                "fetched {key} ({} bytes) to {}",
                                read.record.size,
                                file.display()
                            );
                        }
                        read
                    }
                    Err(e) => {
                        drop(sink);
                        remove_partial(file);
                        return Err(client_err(e)).with_context(|| {
                            format!("fetching {key}; partial output {} removed", file.display())
                        });
                    }
                }
            };
            // The data is correct, but only by reconstruction (SPEC 11.4)
            // or without every record copy (9.4.4): say so, and exit 2 so
            // a pipeline notices.
            if !read.reconstructed.is_empty() {
                eprintln!("{}", describe_reconstruction(key, &read.reconstructed));
            }
            if !read.missing_records.is_empty() {
                eprintln!(
                    "{}",
                    describe_missing_records(key, &read.record, &read.missing_records)
                );
            }
            if !read.reconstructed.is_empty() || !read.missing_records.is_empty() {
                std::process::exit(2);
            }
        }
        Command::Head { key } => {
            let mut client = connect(&cli).await?;
            let read = client.head(key).await.map_err(client_err)?;
            let record = &read.record;
            {
                {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&record)?);
                    } else {
                        println!("key           {}", record.key);
                        println!("version       {}", record.version);
                        println!("size          {} bytes", record.size);
                        println!("created       {}", record.created);
                        println!("checksum      {}", record.object_checksum.to_hex());
                        println!(
                            "scheme        {}+{}, block {} bytes",
                            record.k, record.m, record.block_size
                        );
                        if let Some(ct) = &record.content_type {
                            println!("content-type  {ct}");
                        }
                        for shard in &record.shards {
                            println!("shard {:<3}     device {}", shard.index, shard.device.0);
                        }
                    }
                }
            }
            // The record was trusted without every copy (SPEC 9.4.4): say
            // so, and exit 2 as `get` does.
            if !read.missing_records.is_empty() {
                eprintln!(
                    "{}",
                    describe_missing_records(key, record, &read.missing_records)
                );
                std::process::exit(2);
            }
        }
        Command::Delete { key } => {
            let mut client = connect(&cli).await?;
            client.delete(key).await.map_err(client_err)?;
            if !cli.json {
                println!("deleted {key}");
            }
        }
        Command::List {
            prefix,
            start_after,
            limit,
        } => {
            let mut client = connect(&cli).await?;
            let djbod_client::ListPage { keys, truncated } = client
                .list(ListQuery {
                    prefix: prefix.clone(),
                    start_after: start_after.clone(),
                    limit: *limit,
                })
                .await
                .map_err(client_err)?;
            {
                {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "keys": keys,
                                "truncated": truncated,
                            }))?
                        );
                    } else {
                        print!("{}", tables::list(&keys));
                        if truncated {
                            eprintln!(
                                "(more keys follow; use --start-after {:?})",
                                keys.last().map(|k| k.key.as_str()).unwrap_or("")
                            );
                        }
                    }
                }
            }
        }
        Command::Scrub { rate_mib, repair } => {
            use djbod_proto::message::ScrubEvent;
            let mut client = connect(&cli).await?;
            let mut run = client
                .scrub(rate_mib.map(|m| m * 1024 * 1024), *repair)
                .await
                .map_err(client_err)?;
            let mut findings = 0usize;
            let mut repairs = 0usize;
            let mut repair_failures = 0usize;
            let mut unavailable_devices = 0usize;
            let end = loop {
                match run.next_event().await.map_err(client_err)? {
                    Ok(event) => {
                        // Counted whatever the output mode: the exit code
                        // depends on it (SPEC 20.1.2.3). A device that could
                        // not be read is not damage found but data not
                        // checked: it makes the run incomplete (5.6).
                        match &event {
                            ScrubEvent::NodeFinding {
                                finding: djbod_core::scrub::Finding::DeviceUnavailable { .. },
                                ..
                            } => unavailable_devices += 1,
                            ScrubEvent::NodeFinding { .. } | ScrubEvent::ClusterFinding(_) => {
                                findings += 1
                            }
                            ScrubEvent::Repaired { .. } => repairs += 1,
                            ScrubEvent::RepairFailed { .. } => repair_failures += 1,
                            _ => {}
                        }
                        if cli.json {
                            println!("{}", serde_json::to_string(&event)?);
                            continue;
                        }
                        match &event {
                            ScrubEvent::NodeFinding {
                                node,
                                device,
                                finding,
                            } => {
                                println!(
                                    "node {}  device {}\n  {}",
                                    short(&node.0),
                                    short(&device.0),
                                    describe_scrub_finding(finding)
                                );
                            }
                            ScrubEvent::NodeSummary {
                                node,
                                device,
                                summary,
                            } => {
                                eprintln!(
                                    "node {}  device {}: {} records, {} shards, {} blocks, {} read, {} finding(s)",
                                    short(&node.0),
                                    short(&device.0),
                                    summary.records_checked,
                                    summary.shards_checked,
                                    summary.blocks_checked,
                                    human_bytes(summary.bytes_read),
                                    summary.findings.len()
                                );
                            }
                            ScrubEvent::NodeFailed { node, detail } => {
                                println!(
                                    "node {} could not be scrubbed: {}",
                                    short(&node.0),
                                    detail.message
                                );
                            }
                            ScrubEvent::ClusterFinding(finding) => {
                                println!("cluster check: {finding:?}");
                            }
                            ScrubEvent::CrossCheckStopped {
                                node,
                                detail,
                                versions_checked,
                                versions_unchecked,
                            } => {
                                let unchecked = match versions_unchecked {
                                    Some(count) => format!("{count} not checked"),
                                    None => "the rest not checked".to_string(),
                                };
                                println!(
                                    "cross-node checks stopped at node {}: {}; {versions_checked} version(s) checked, {unchecked}",
                                    short(&node.0),
                                    detail.message
                                );
                            }
                            ScrubEvent::CrossCheckProgress {
                                versions_checked, ..
                            } => {
                                eprintln!(
                                    "cross-node checks: {versions_checked} version(s) checked"
                                );
                            }
                            ScrubEvent::Repaired { key, report } => {
                                let rewritten =
                                    report.shards.iter().filter(|s| s.rewritten).count();
                                println!("repaired {key}: {rewritten} shard(s) rewritten");
                            }
                            ScrubEvent::RepairFailed { key, detail } => {
                                println!("repair of {key} failed: {}", describe_detail(detail));
                            }
                        }
                    }
                    Err(end) => break end,
                }
            };
            // Incomplete means a node could not be scrubbed, the checks
            // stopped, or a device could not be read; failed repairs end
            // the stream with WriteFailed and the run is still complete.
            let incomplete = unavailable_devices > 0
                || end
                    .error
                    .as_ref()
                    .is_some_and(|e| e.code != djbod_proto::message::ErrorCode::WriteFailed);
            let code = scrub_exit_code(*repair, incomplete, findings, repair_failures);
            if !cli.json {
                eprintln!(
                    "{findings} finding(s), {repairs} repair(s), {repair_failures} failed repair(s): {}",
                    scrub_outcome(*repair, incomplete, findings, repair_failures)
                );
                if unavailable_devices > 0 {
                    eprintln!(
                        "{unavailable_devices} device(s) unavailable, not checked: restore or retire them, then run again"
                    );
                }
                if let Some(error) = &end.error {
                    eprintln!("scrub incomplete: {}", describe_detail(error));
                }
            }
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::MoveShard {
            key,
            shard_index,
            to,
        } => {
            let target = match to {
                Some(name) => Some(resolve_device(&cli, name).await?),
                None => None,
            };
            let mut client = connect(&cli).await?;
            let djbod_client::MoveShardReport {
                record,
                source,
                source_cleaned,
                rebuilt,
            } = client
                .move_shard(key, *shard_index, target)
                .await
                .map_err(client_err)?;
            {
                {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "record": record,
                                "source": source,
                                "source_cleaned": source_cleaned,
                                "rebuilt": rebuilt,
                            }))?
                        );
                    } else {
                        let destination = record
                            .device_for(djbod_core::erasure::ShardIndex(*shard_index))
                            .map(|d| d.0.to_string())
                            .unwrap_or_default();
                        println!(
                            "moved shard {shard_index} of {key} from {} to {destination} ({}); record now revision {}{}",
                            source.0,
                            if rebuilt { "rebuilt from the other shards" } else { "copied" },
                            record.revision,
                            if source_cleaned { "" } else { "; source copy not removed, scrub will report it as stale" }
                        );
                    }
                }
            }
        }
        Command::Repair { key } => {
            let mut client = connect(&cli).await?;
            let report = client.repair(key).await.map_err(client_err)?;
            {
                {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!("key      {}", report.key);
                        println!("version  {}", report.version);
                        for shard in &report.shards {
                            let condition = match &shard.condition {
                                djbod_proto::message::ShardCondition::Intact => {
                                    "intact".to_string()
                                }
                                djbod_proto::message::ShardCondition::Unreadable { reason } => {
                                    format!("unreadable ({reason})")
                                }
                                djbod_proto::message::ShardCondition::CorruptBlocks { stripes } => {
                                    format!("corrupt blocks in stripes {stripes:?}")
                                }
                                djbod_proto::message::ShardCondition::Lost => {
                                    "device no longer in the cluster".to_string()
                                }
                            };
                            let outcome = match (&shard.relocated_to, shard.rewritten) {
                                (Some(device), _) => format!("  -> rebuilt on {}", device.0),
                                (None, true) => "  -> rewritten".to_string(),
                                (None, false) => String::new(),
                            };
                            println!(
                                "shard {:<3}  device {}  {condition}{outcome}",
                                shard.index, shard.device.0
                            );
                        }
                        let count = report.shards.iter().filter(|s| s.rewritten).count();
                        println!("{count} shard(s) rewritten");
                    }
                }
            }
        }
        Command::Cluster { command } => {
            let (node, cluster) = reachable_node(&cli).await?;
            match command {
                ClusterCommand::Show => {
                    let document =
                        djbod_client::admin::fetch_document(&connector(&cli)?, node, cluster)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let reports =
                        djbod_client::admin::fetch_all(&connector(&cli)?, &document).await;
                    if cli.json {
                        let rows: Vec<serde_json::Value> = reports
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "node": r.node,
                                    "label": r.label,
                                    "address": r.address,
                                    "addresses": document.node(r.node).map(|n| &n.addresses),
                                    "build": r.build,
                                    "version": r.result.as_ref().ok().map(|d| d.version),
                                    "error": r.result.as_ref().err(),
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    } else {
                        println!("cluster   {}", document.title());
                        println!("document  version {} as held by {node}", document.version);
                        println!();
                        print!("{}", tables::cluster_show(&document, &reports));
                    }
                }
                ClusterCommand::SetState { device, state } => {
                    let state = match state {
                        StateArg::Draining => DeviceState::Draining,
                        StateArg::Active => DeviceState::Active,
                    };
                    let device_id = resolve_device(&cli, device).await?;
                    let (document, changed) = djbod_client::admin::set_device_state(
                        &connector(&cli)?,
                        node,
                        cluster,
                        device_id,
                        state,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let state_name = format!("{state:?}").to_lowercase();
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "device": device,
                                "state": state_name,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if changed {
                        println!(
                            "device {} is now {state_name} (document version {})",
                            device_id.0, document.version
                        );
                    } else {
                        println!(
                            "device {} was already {state_name}; nothing changed",
                            device_id.0
                        );
                    }
                }
                ClusterCommand::Drain {
                    device,
                    node_id,
                    partial,
                } => {
                    let devices: Vec<Uuid> = match (device, node_id) {
                        (Some(device), _) => vec![resolve_device(&cli, device).await?.0],
                        (None, Some(node_id)) => {
                            let document = djbod_client::admin::fetch_document(
                                &connector(&cli)?,
                                node,
                                cluster,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                            let wanted =
                                document
                                    .node_by_name(node_id)
                                    .map(|n| n.id)
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "no node is named {node_id:?}, as a UUID or a label"
                                        )
                                    })?;
                            let found: Vec<Uuid> = document
                                .devices
                                .iter()
                                .filter(|d| d.node == wanted && d.state == DeviceState::Draining)
                                .map(|d| d.id.0)
                                .collect();
                            if found.is_empty() {
                                println!("node {node_id} has no draining devices");
                            }
                            found
                        }
                        (None, None) => unreachable!("clap requires one of the two"),
                    };
                    let mut all_moved = true;
                    for device in devices {
                        all_moved &= drain_device(&cli, DeviceId(device), *partial).await?;
                    }
                    if !all_moved {
                        std::process::exit(2);
                    }
                }
                ClusterCommand::SetScheme { k, m, block_size } => {
                    let (document, changed) = djbod_client::admin::set_scheme(
                        &connector(&cli)?,
                        node,
                        cluster,
                        *k,
                        *m,
                        *block_size,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let behind = count_versions_behind(&cli, &document).await?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "k": document.k,
                                "m": document.m,
                                "block_size": document.block_size,
                                "document_version": document.version,
                                "changed": changed,
                                "objects_at_other_schemes": behind,
                            }))?
                        );
                    } else {
                        if changed {
                            println!(
                                "scheme is now {}+{} with {} byte blocks (document version {}); new writes use it",
                                document.k, document.m, document.block_size, document.version
                            );
                        } else {
                            println!(
                                "scheme was already {}+{} with {} byte blocks; nothing changed",
                                document.k, document.m, document.block_size
                            );
                        }
                        if behind > 0 {
                            println!(
                                "{behind} object(s) are stored at another scheme and stay readable as they are; `djbod cluster reencode` rewrites them"
                            );
                        } else {
                            println!("every object is at this scheme");
                        }
                    }
                }
                ClusterCommand::SetTransport { transport } => {
                    let transport = match transport {
                        DeviceTransport::Plain => djbod_core::cluster::Transport::Plain,
                        DeviceTransport::TlsOptional => djbod_core::cluster::Transport::TlsOptional,
                        DeviceTransport::Tls => djbod_core::cluster::Transport::Tls,
                    };
                    let (document, changed) = djbod_client::admin::set_transport(
                        &connector(&cli)?,
                        node,
                        cluster,
                        transport,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "transport": document.transport.to_string(),
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if changed {
                        println!(
                            "transport is now {} (document version {})",
                            document.transport, document.version
                        );
                    } else {
                        println!(
                            "transport was already {}; nothing changed",
                            document.transport
                        );
                    }
                }
                ClusterCommand::SetLimits {
                    max_key_bytes,
                    max_object_bytes,
                    max_user_metadata_bytes,
                } => {
                    let (document, changed) = djbod_client::admin::set_limits(
                        &connector(&cli)?,
                        node,
                        cluster,
                        *max_key_bytes,
                        *max_object_bytes,
                        *max_user_metadata_bytes,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "max_key_bytes": document.max_key_bytes,
                                "max_object_bytes": document.max_object_bytes,
                                "max_user_metadata_bytes": document.max_user_metadata_bytes,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else {
                        println!(
                            "max key length {} bytes, max object size {} bytes, max user metadata {} bytes (document version {}){}",
                            document.max_key_bytes,
                            document.max_object_bytes,
                            document.max_user_metadata_bytes,
                            document.version,
                            if changed { "" } else { "; nothing changed" }
                        );
                    }
                }
                ClusterCommand::Reencode => {
                    let document =
                        djbod_client::admin::fetch_document(&connector(&cli)?, node, cluster)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let failures = reencode_all(&cli, &document).await?;
                    if failures > 0 {
                        std::process::exit(2);
                    }
                }
                ClusterCommand::SetLabel {
                    device,
                    label,
                    clear: _,
                } => {
                    let device_id = resolve_device(&cli, device).await?;
                    let (document, changed) = djbod_client::admin::set_device_label(
                        &connector(&cli)?,
                        node,
                        cluster,
                        device_id,
                        label.clone(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "device": device_id,
                                "label": label,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if !changed {
                        println!("nothing changed");
                    } else if let Some(label) = label {
                        println!(
                            "device {} is now labelled {label} (document version {})",
                            device_id.0, document.version
                        );
                    } else {
                        println!(
                            "label cleared from device {} (document version {})",
                            device_id.0, document.version
                        );
                    }
                }
                ClusterCommand::SetName { name, clear: _ } => {
                    let (document, changed) = djbod_client::admin::set_cluster_name(
                        &connector(&cli)?,
                        node,
                        cluster,
                        name.clone(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "cluster_id": cluster,
                                "name": name,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if !changed {
                        println!("nothing changed");
                    } else if let Some(name) = name {
                        println!(
                            "cluster {cluster} is now named {name} (document version {})",
                            document.version
                        );
                    } else {
                        println!(
                            "name cleared from cluster {cluster} (document version {})",
                            document.version
                        );
                    }
                }
                ClusterCommand::SetNodeLabel {
                    node_id,
                    label,
                    clear: _,
                } => {
                    let id = resolve_node(&cli, node_id).await?;
                    let (document, changed) = djbod_client::admin::set_node_label(
                        &connector(&cli)?,
                        node,
                        cluster,
                        id,
                        label.clone(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "node": id,
                                "label": label,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if !changed {
                        println!("nothing changed");
                    } else if let Some(label) = label {
                        println!(
                            "node {} is now labelled {label} (document version {})",
                            id.0, document.version
                        );
                    } else {
                        println!(
                            "label cleared from node {} (document version {})",
                            id.0, document.version
                        );
                    }
                }
                ClusterCommand::SetAddress { node_id, addresses } => {
                    let id = resolve_node(&cli, node_id).await?;
                    let (document, changed) = djbod_client::admin::set_node_addresses(
                        &connector(&cli)?,
                        node,
                        cluster,
                        id,
                        addresses.clone(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "node": id,
                                "addresses": addresses,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if !changed {
                        println!("nothing changed");
                    } else {
                        println!(
                            "node {} is now reached at {} (document version {})",
                            id.0,
                            addresses.join(", "),
                            document.version
                        );
                    }
                }
                ClusterCommand::RemoveDevice {
                    device,
                    force: true,
                    yes,
                } => {
                    let device_id = resolve_device(&cli, device).await?;
                    force_remove_device(&cli, node, cluster, device_id, *yes).await?
                }
                ClusterCommand::RemoveDevice {
                    device,
                    force: false,
                    ..
                } => {
                    let device_id = resolve_device(&cli, device).await?;
                    let (document, changed) = djbod_client::admin::remove_device(
                        &connector(&cli)?,
                        node,
                        cluster,
                        device_id,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "device": device,
                                "document_version": document.version,
                                "changed": changed,
                            }))?
                        );
                    } else if changed {
                        println!(
                            "device {device} removed (document version {}); take it out of its node's configuration and restart that node",
                            document.version
                        );
                    } else {
                        println!("device {device} was already removed; nothing changed");
                    }
                }
                ClusterCommand::RemoveNode {
                    node_id,
                    force: false,
                    ..
                } => {
                    let id = resolve_node(&cli, node_id).await?;
                    let document =
                        djbod_client::admin::remove_node(&connector(&cli)?, node, cluster, id)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "node": id,
                                "document_version": document.version,
                            }))?
                        );
                    } else {
                        println!(
                            "node {} removed (document version {}); its process stops on its own, and its devices can be reused with `djbod-node join --wipe-removed-device`",
                            id.0, document.version
                        );
                    }
                }
                ClusterCommand::RemoveNode {
                    node_id,
                    force: true,
                    yes,
                } => {
                    let id = resolve_node(&cli, node_id).await?;
                    force_remove_node(&cli, node, cluster, id, *yes).await?
                }
                ClusterCommand::Sync => {
                    let report = djbod_client::admin::sync(&connector(&cli)?, node, cluster)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "highest_version": report.highest_version,
                                "updated": report.updated,
                                "already_current": report.already_current,
                                "unreachable": report.unreachable,
                            }))?
                        );
                    } else {
                        println!("highest version  {}", report.highest_version);
                        for n in &report.updated {
                            println!("updated          {}", n.0);
                        }
                        for n in &report.already_current {
                            println!("already current  {}", n.0);
                        }
                        for (n, reason) in &report.unreachable {
                            println!("unreachable      {}  {reason}", n.0);
                        }
                        if !report.unreachable.is_empty() {
                            std::process::exit(2);
                        }
                    }
                }
            }
        }
        Command::ClusterConfig => {
            let mut client = connect(&cli).await?;
            let document = client.cluster_document().await.map_err(client_err)?;
            println!("{}", serde_json::to_string_pretty(&document)?);
        }
    }
    Ok(())
}

/// Whether a version is stored at the document's scheme and block size.
fn at_current_scheme(
    record: &djbod_core::record::MetadataRecord,
    document: &djbod_core::cluster::ClusterDocument,
) -> bool {
    record.k == document.k && record.m == document.m && record.block_size == document.block_size
}

/// Every key in the cluster, in pages.
async fn all_keys(cli: &Cli) -> anyhow::Result<Vec<String>> {
    let mut client = connect(cli).await?;
    let keys = client.list_all(None).await.map_err(client_err)?;
    Ok(keys.into_iter().map(|e| e.key).collect())
}

/// How many objects are stored at a scheme or block size other than the
/// document's.
async fn count_versions_behind(
    cli: &Cli,
    document: &djbod_core::cluster::ClusterDocument,
) -> anyhow::Result<usize> {
    let mut client = connect(cli).await?;
    let mut behind = 0usize;
    for key in all_keys(cli).await? {
        let record = client.head(&key).await.map_err(client_err)?.record;
        if !at_current_scheme(&record, document) {
            behind += 1;
        }
    }
    Ok(behind)
}

/// The migration of SPEC 18.9: every version whose recorded scheme or
/// block size differs from the document's is read once and written back
/// as a new version under the same key, streamed through this process,
/// keeping its content type and user metadata; the write replaces the old
/// version. Returns how many objects could not be re-encoded.
async fn reencode_all(
    cli: &Cli,
    document: &djbod_core::cluster::ClusterDocument,
) -> anyhow::Result<usize> {
    let mut lister = connect(cli).await?;
    let mut examined = 0usize;
    let mut reencoded = 0usize;
    let mut failures = 0usize;
    for key in all_keys(cli).await? {
        {
            examined += 1;
            let record = match lister.head(&key).await {
                Ok(read) => read.record,
                Err(e) => {
                    failures += 1;
                    println!("FAILED   {key}  head: {}", client_err(e));
                    continue;
                }
            };
            if at_current_scheme(&record, document) {
                continue;
            }
            match reencode_one(cli, &record).await {
                Ok(new_version) => {
                    reencoded += 1;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "key": record.key,
                                "from": format!("{}+{}", record.k, record.m),
                                "to": format!("{}+{}", document.k, document.m),
                                "old_version": record.version.to_text(),
                                "new_version": new_version.to_text(),
                            })
                        );
                    } else {
                        println!(
                            "re-encoded  {}  {}+{} -> {}+{}  (version {} -> {new_version})",
                            record.key, record.k, record.m, document.k, document.m, record.version
                        );
                    }
                }
                Err(e) => {
                    failures += 1;
                    println!("FAILED   {}  {e:#}", record.key);
                }
            }
        }
    }
    if !cli.json {
        eprintln!("{examined} object(s) examined, {reencoded} re-encoded, {failures} failed");
    }
    Ok(failures)
}

/// Read one version and write it back under the same key: a GET streamed
/// into a PUT through an in-process pipe, so no temporary file is needed
/// and the object is verified on the way out and in.
async fn reencode_one(
    cli: &Cli,
    record: &djbod_core::record::MetadataRecord,
) -> anyhow::Result<djbod_core::version::VersionId> {
    // Two clients, since the read and the write run at once.
    let mut reader = connect(cli).await?;
    let mut writer = connect(cli).await?;
    let (mut pipe_in, mut pipe_out) = tokio::io::duplex(4 * 1024 * 1024);
    let key = record.key.clone();
    let get = tokio::spawn(async move { reader.get_to_writer(&key, &mut pipe_in).await });
    let put = writer
        .put_from_reader(
            &record.key,
            record.size,
            &mut pipe_out,
            record.content_type.clone(),
            record.user_metadata.clone(),
        )
        .await;
    let got = get.await.context("the read task failed")?;
    match (got, put) {
        (Ok(read), Ok(write)) => {
            if read.record.version != record.version {
                bail!(
                    "the object changed while being re-encoded (read version {}, expected {}); rerun",
                    read.record.version,
                    record.version
                );
            }
            Ok(write.version)
        }
        (Err(e), _) => Err(anyhow::anyhow!("read failed: {}", client_err(e))),
        (Ok(_), Err(e)) => Err(anyhow::anyhow!("write failed: {}", client_err(e))),
    }
}

/// `cluster remove-device --force` (SPEC 18.2.1.1): say what it means,
/// confirm, mark the device removed. Nothing is scanned and nothing is
/// moved; `scrub --repair` rebuilds what the device held.
async fn force_remove_device(
    cli: &Cli,
    peer: SocketAddr,
    cluster: Uuid,
    device_id: DeviceId,
    yes: bool,
) -> anyhow::Result<()> {
    use djbod_client::admin;
    let mut client = connect(cli).await?;
    let status = client.status().await.map_err(client_err)?;
    let document = client.cluster_document().await.map_err(client_err)?;
    let entry = document
        .device(device_id)
        .ok_or_else(|| anyhow::anyhow!("device {device_id} is not in the cluster document"))?;
    let m = document.m;
    let needed = document.k as usize + document.m as usize;
    // Devices that could take a rebuilt shard once this one is gone.
    let remaining = status
        .devices
        .iter()
        .filter(|d| d.device != device_id && d.state == DeviceState::Active && d.available)
        .count();
    let readable = status
        .devices
        .iter()
        .any(|d| d.device == device_id && d.available);
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "device": device_id,
                "node": entry.node,
                "state": format!("{:?}", entry.state).to_lowercase(),
                "readable": readable,
                "remaining_active_devices": remaining,
                "needed_per_version": needed,
            }))?
        );
    } else {
        println!(
            "device {} on node {} is {}{}",
            device_id.0,
            entry.node.0,
            format!("{:?}", entry.state).to_lowercase(),
            if readable {
                " and its node can still read it"
            } else {
                " and its node cannot read it"
            }
        );
        println!(
            "marking it removed loses every shard on it: versions with at most m = {m} shards there are rebuilt from the others by `djbod scrub --repair`; any with more are lost. Nothing is checked or moved now."
        );
        if remaining < needed {
            println!(
                "warning: {remaining} active device(s) would remain and every version needs {needed}; nothing can be rebuilt until a device is added"
            );
        }
    }
    if !yes {
        eprint!("type the device id to mark it removed: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != device_id.0.to_string() {
            bail!("confirmation did not match; nothing changed");
        }
    }
    let (document, changed) =
        admin::remove_device_forced(&connector(cli)?, peer, cluster, device_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "device": device_id,
                "document_version": document.version,
                "changed": changed,
            }))?
        );
    } else if changed {
        println!(
            "device {} removed (document version {}); run `djbod scrub --repair` to rebuild what it held, then take it out of its node's configuration and restart that node",
            device_id.0, document.version
        );
    } else {
        println!(
            "device {} was already removed; nothing changed",
            device_id.0
        );
    }
    Ok(())
}

/// `cluster remove-node --force` (SPEC 6.2.6.3): show the cost, confirm,
/// propose without the dead node, then rebuild what it held.
async fn force_remove_node(
    cli: &Cli,
    peer: SocketAddr,
    cluster: Uuid,
    node_id: NodeId,
    yes: bool,
) -> anyhow::Result<()> {
    use djbod_client::admin;
    let plan = admin::plan_forced_removal(&connector(cli)?, peer, cluster, node_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let unrecoverable = plan.unrecoverable();
    let m = plan.current.m;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": node_id,
                "address": plan.address,
                "unreachable_because": plan.unreachable_because,
                "devices": plan.devices,
                "affected_versions": plan.affected.len(),
                "unrecoverable_keys": unrecoverable.iter().map(|r| &r.key).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!(
            "node {} at {} does not answer: {}",
            node_id.0, plan.address, plan.unreachable_because
        );
        println!(
            "it holds {} device(s); {} version(s) have shards there",
            plan.devices.len(),
            plan.affected.len()
        );
        if unrecoverable.is_empty() {
            println!("every one of them can be rebuilt from the other shards (at most m = {m} on the dead node)");
        } else {
            println!(
                "{} of them have more than m = {m} shards there and CANNOT be rebuilt; they will be lost:",
                unrecoverable.len()
            );
            for r in &unrecoverable {
                println!("  {}  ({} shards on the dead node)", r.key, r.shards);
            }
        }
    }
    if !yes {
        eprint!("type the node id to remove it without its acknowledgement: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != node_id.0.to_string() {
            bail!("confirmation did not match; nothing changed");
        }
    }
    let document = admin::execute_forced_removal(&connector(cli)?, &plan)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !cli.json {
        println!(
            "node {} removed (document version {}); rebuilding {} version(s)",
            node_id.0,
            document.version,
            plan.affected.len()
        );
    }
    // Step 3: rebuild, one repair per affected key, through a live node.
    let mut client = connect(cli).await?;
    let mut failed = 0usize;
    for reference in &plan.affected {
        match client.repair(&reference.key).await {
            Ok(report) => {
                if cli.json {
                    println!("{}", serde_json::to_string(&report)?);
                } else {
                    let relocated = report
                        .shards
                        .iter()
                        .filter(|s| s.relocated_to.is_some())
                        .count();
                    println!(
                        "rebuilt  {}  {relocated} shard(s) placed on other devices",
                        reference.key
                    );
                }
            }
            Err(e) => {
                failed += 1;
                println!("LOST     {}  {}", reference.key, client_err(e));
            }
        }
    }
    if !cli.json {
        eprintln!("{} rebuilt, {failed} lost", plan.affected.len() - failed);
    }
    if failed > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// Run one drain and print its progress. Returns whether every version
/// was moved.
async fn drain_device(cli: &Cli, device: DeviceId, partial: bool) -> anyhow::Result<bool> {
    let mut client = connect(cli).await?;
    let mut run = client.drain(device, partial).await.map_err(client_err)?;
    let mut moved = 0usize;
    let mut deleted = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    let end = loop {
        match run.next_event().await.map_err(client_err)? {
            Ok(event) => {
                if cli.json {
                    println!("{}", serde_json::to_string(&event)?);
                    match &event {
                        DrainEvent::Skipped { key, detail, .. } => {
                            skipped.push((key.clone(), detail.message.clone()));
                        }
                        DrainEvent::Deleted { .. } => deleted += 1,
                        _ => {}
                    }
                    continue;
                }
                match event {
                    DrainEvent::Estimate {
                        device,
                        node,
                        versions,
                        shard_bytes,
                        target_free_bytes,
                        active_devices,
                        required_devices,
                    } => {
                        println!(
                            "draining {} on node {}: {versions} version(s), {} to move; {} free on {active_devices} active device(s), {required_devices} needed per version",
                            device.0,
                            short(&node.0),
                            human_bytes(shard_bytes),
                            human_bytes(target_free_bytes)
                        );
                    }
                    DrainEvent::Moved {
                        key,
                        shard_index,
                        destination,
                        rebuilt,
                        ..
                    } => {
                        moved += 1;
                        println!(
                            "moved    {key}  shard {shard_index} -> {}{}",
                            destination.0,
                            if rebuilt { " (rebuilt)" } else { "" }
                        );
                    }
                    DrainEvent::Skipped { key, detail, .. } => {
                        println!("skipped  {key}  {}", describe_detail(&detail));
                        skipped.push((key, detail.message));
                    }
                    DrainEvent::Deleted { key, .. } => {
                        deleted += 1;
                        println!("deleted  {key}  (removed since the pass began; nothing to move)");
                    }
                }
            }
            Err(end) => break end,
        }
    };
    if !cli.json {
        eprintln!(
            "{moved} moved, {} skipped, {deleted} deleted meanwhile",
            skipped.len()
        );
    }
    if let Some(error) = &end.error {
        eprintln!(
            "drain of {} incomplete: {}",
            device.0,
            describe_detail(error)
        );
        return Ok(false);
    }
    Ok(true)
}

fn short(id: &Uuid) -> String {
    id.to_string()[..8].to_string()
}

/// The record copies a read went without (SPEC 9.4.4): the record was
/// trusted on the copies that agreed, and `repair` rewrites the rest.
/// A copy on a device that is out is nothing to repair; the device is.
fn describe_missing_records(
    key: &str,
    record: &djbod_core::record::MetadataRecord,
    missing: &[MissingRecordCopy],
) -> String {
    let mut lines = vec![format!(
        "{key}: {} of {} record copies could not be read; the record was trusted on the copies that agree, and `djbod repair {key}` rewrites the missing ones",
        missing.len(),
        record.k as usize + record.m as usize
    )];
    for copy in missing {
        let fault = match &copy.fault {
            RecordCopyFault::Missing => "missing".to_string(),
            RecordCopyFault::Stale { revision } => {
                format!("stale, at revision {revision}: an interrupted re-placement")
            }
            RecordCopyFault::Unavailable { reason } => {
                format!("unavailable, perhaps for now: {reason}")
            }
        };
        lines.push(format!("  record copy  device {}  {fault}", copy.device.0));
    }
    lines.join("\n")
}

/// What a read had to reconstruct (SPEC 11.4): the bytes returned are
/// correct, the damage on disk is not fixed, and every read pays again
/// until it is.
fn describe_reconstruction(key: &str, reconstructed: &[Reconstruction]) -> String {
    let mut lines = vec![format!(
        "{key}: {} block(s) reconstructed from parity; the data is correct, the damage on disk is not repaired, and every read pays again until `djbod repair {key}` runs",
        reconstructed.len()
    )];
    for r in reconstructed {
        let fault = match &r.fault {
            FaultKind::Missing => "missing".to_string(),
            FaultKind::WrongLength { expected, actual } => {
                format!("wrong length: {actual} bytes, {expected} expected")
            }
            FaultKind::ChecksumMismatch { .. } => "checksum mismatch".to_string(),
            FaultKind::Unreadable { reason } => format!("unreadable: {reason}"),
            FaultKind::Unavailable { reason } => {
                format!("unavailable, perhaps for now: {reason}")
            }
        };
        let where_ = if r.stripes == 1 {
            format!("stripe {}", r.first_stripe)
        } else {
            format!(
                "stripes {}..{}",
                r.first_stripe,
                r.first_stripe + r.stripes - 1
            )
        };
        lines.push(format!(
            "  {where_}  shard {}  device {}  {fault}",
            r.shard_index, r.device.0
        ));
    }
    lines.join("\n")
}

fn describe_scrub_finding(finding: &djbod_core::scrub::Finding) -> String {
    use djbod_core::scrub::Finding::*;
    match finding {
        RecordCorrupt { path, reason } => format!("record corrupt: {}  {reason}", path.display()),
        ShardUnreadable { path, reason, .. } => {
            format!("shard unreadable: {}  {reason}", path.display())
        }
        ShardMisplaced { path, reason, .. } => {
            format!("shard misplaced: {}  {reason}", path.display())
        }
        ShardBlocksCorrupt { path, stripes, .. } => {
            format!("corrupt blocks: {}  stripes {stripes:?}", path.display())
        }
        ShardWithoutRecord { path, .. } => format!("shard without record: {}", path.display()),
        RecordWithoutShard {
            path, shard_index, ..
        } => format!("record without shard {shard_index}: {}", path.display()),
        RecordNotForThisDevice { path, .. } => {
            format!("record not for this device: {}", path.display())
        }
        StaleTemporary { path, age_secs } => {
            format!("stale temporary: {}  {age_secs}s old", path.display())
        }
        DeviceUnavailable { reason } => format!("device unavailable: {reason}"),
    }
}

fn remove_partial(path: &Path) {
    let _ = std::fs::remove_file(path);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_text_names_the_client_only_when_it_differs() {
        assert_eq!(build_text("0.1.0+abc", "0.1.0+abc"), "0.1.0+abc");
        assert_eq!(
            build_text("0.1.0+abc", "0.1.0+def"),
            "0.1.0+abc (this client: 0.1.0+def)"
        );
    }

    /// SPEC 20.1.2.3: the four outcomes and their codes, for both forms.
    #[test]
    fn scrub_exit_codes_follow_the_four_outcomes() {
        // (repair, incomplete, findings, repair_failures) -> code
        let cases = [
            (false, false, 0, 0, 0),
            (false, false, 3, 0, 2),
            (false, true, 0, 0, 3),
            (false, true, 3, 0, 4),
            (true, false, 0, 0, 0),
            (true, false, 3, 0, 0), // everything found was repaired
            (true, false, 3, 1, 2), // one repair failed
            (true, true, 0, 0, 3),
            (true, true, 3, 0, 4), // repaired, but the scan did not finish
            (true, true, 3, 2, 4),
        ];
        for (repair, incomplete, findings, failures, code) in cases {
            assert_eq!(
                scrub_exit_code(repair, incomplete, findings, failures),
                code,
                "repair={repair} incomplete={incomplete} findings={findings} failures={failures}"
            );
        }
    }
}
