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

use djbod_core::cluster::{DeviceState, NodeId};
use djbod_core::record::DeviceId;
use djbod_node::client::{ClientError, Connection, DEFAULT_BODY_CHUNK};
use djbod_node::transport::{ClientTlsPaths, Connector};
use djbod_proto::message::{DrainEvent, ErrorDetail, ListQuery, Request, Response};

#[derive(Parser)]
#[command(name = "djbod", about = "Distributed-JBOD client", version)]
struct Cli {
    /// Address of any node in the cluster.
    #[arg(long, env = "DJBOD_NODE", global = true)]
    node: Option<SocketAddr>,
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

/// How this client connects (SPEC 19.1.6.2): TLS when a CA is given,
/// with a certificate when one is given too, plain otherwise.
fn connector(cli: &Cli) -> anyhow::Result<Connector> {
    match &cli.tls_ca {
        None => Ok(Connector::plain()),
        Some(ca) => {
            let identity = match (&cli.tls_cert, &cli.tls_key) {
                (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
                _ => None,
            };
            Connector::from_client_paths(&ClientTlsPaths {
                ca: ca.clone(),
                identity,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show every device's state and free space.
    Status,
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
        /// Destination device id; chosen like a write if omitted.
        #[arg(long)]
        to: Option<Uuid>,
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
    /// Moves no data.
    SetState { device: Uuid, state: StateArg },
    /// Move every shard off a draining device in one pass.
    Drain {
        /// The draining device to empty.
        #[arg(required_unless_present = "node_id", conflicts_with = "node_id")]
        device: Option<Uuid>,
        /// Drain every draining device of this node in turn.
        #[arg(long = "node-id", value_name = "NODE")]
        node_id: Option<Uuid>,
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
    /// on it: drain it first.
    RemoveDevice { device: Uuid },
    /// Drop a node and its devices from the cluster. Refused while any
    /// object still has a shard on them: drain them first. The node stops
    /// serving once it has acknowledged.
    RemoveNode {
        node_id: Uuid,
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

async fn connect(cli: &Cli) -> anyhow::Result<Connection> {
    let node = cli
        .node
        .context("no node address: pass --node or set DJBOD_NODE")?;
    let cluster = cli
        .cluster
        .context("no cluster id: pass --cluster or set DJBOD_CLUSTER")?;
    Connection::connect_with(&connector(cli)?, node, Connection::client_hello(cluster))
        .await
        .map_err(|e| anyhow::anyhow!("{}", describe_error(&e)))
        .with_context(|| format!("connecting to {node}"))
}

/// Render a client error the way an administrator wants to read it: the
/// code, the message, then every identifying field the node supplied
/// (SPEC 16.2).
fn describe_error(e: &ClientError) -> String {
    match e {
        ClientError::Remote(detail) | ClientError::StreamFailed(detail) => describe_detail(detail),
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

fn remote(e: ClientError) -> anyhow::Error {
    anyhow::anyhow!("{}", describe_error(&e))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Status => {
            let mut conn = connect(&cli).await?;
            match conn.request(Request::Status).await.map_err(remote)? {
                Response::Status {
                    cluster_id,
                    document_version,
                    coordinator,
                    transport,
                    devices,
                } => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "cluster_id": cluster_id,
                                "document_version": document_version,
                                "coordinator": coordinator,
                                "transport": transport.to_string(),
                                "devices": devices,
                            }))?
                        );
                    } else {
                        println!("cluster   {cluster_id}");
                        println!("document  version {document_version}");
                        println!("answered  by node {}", coordinator.0);
                        println!("transport {transport}");
                        println!();
                        println!(
                            "{:<36}  {:<36}  {:<9}  {:>12}  {:>12}",
                            "DEVICE", "NODE", "STATE", "TOTAL", "FREE"
                        );
                        for d in devices {
                            println!(
                                "{:<36}  {:<36}  {:<9}  {:>12}  {:>12}",
                                d.device.0,
                                d.node.0,
                                format!("{:?}", d.state).to_lowercase(),
                                human_bytes(d.total_bytes),
                                human_bytes(d.free_bytes)
                            );
                        }
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Put {
            key,
            file,
            content_type,
        } => {
            let mut conn = connect(&cli).await?;
            let version = if file.as_os_str() == "-" {
                let mut body = Vec::new();
                tokio::io::stdin()
                    .read_to_end(&mut body)
                    .await
                    .context("reading standard input")?;
                conn.put_object(key, &body, DEFAULT_BODY_CHUNK, content_type.clone())
                    .await
                    .map_err(remote)?
            } else {
                let mut source = tokio::fs::File::open(file)
                    .await
                    .with_context(|| format!("opening {}", file.display()))?;
                let size = source.metadata().await?.len();
                conn.put_object_from_reader(
                    key,
                    size,
                    &mut source,
                    DEFAULT_BODY_CHUNK,
                    content_type.clone(),
                )
                .await
                .map_err(remote)?
            };
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "key": key, "version": version.to_text() })
                );
            } else {
                println!("stored {key} as version {version}");
            }
        }
        Command::Get { key, file } => {
            let mut conn = connect(&cli).await?;
            if file.as_os_str() == "-" {
                let mut stdout = tokio::io::stdout();
                conn.get_object_to_writer(key, &mut stdout)
                    .await
                    .map_err(remote)?;
                stdout.flush().await?;
            } else {
                let mut sink = tokio::fs::File::create(file)
                    .await
                    .with_context(|| format!("creating {}", file.display()))?;
                let result = conn.get_object_to_writer(key, &mut sink).await;
                match result {
                    Ok(record) => {
                        sink.sync_all().await?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&record)?);
                        } else {
                            eprintln!(
                                "fetched {key} ({} bytes) to {}",
                                record.size,
                                file.display()
                            );
                        }
                    }
                    Err(e) => {
                        drop(sink);
                        remove_partial(file);
                        return Err(remote(e)).with_context(|| {
                            format!("fetching {key}; partial output {} removed", file.display())
                        });
                    }
                }
            }
        }
        Command::Head { key } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::HeadObject { key: key.clone() })
                .await
                .map_err(remote)?
            {
                Response::HeadObject { record } => {
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
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Delete { key } => {
            let mut conn = connect(&cli).await?;
            conn.request(Request::DeleteObject { key: key.clone() })
                .await
                .map_err(remote)?;
            if !cli.json {
                println!("deleted {key}");
            }
        }
        Command::List {
            prefix,
            start_after,
            limit,
        } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::ListKeys(ListQuery {
                    prefix: prefix.clone(),
                    start_after: start_after.clone(),
                    limit: *limit,
                }))
                .await
                .map_err(remote)?
            {
                Response::ListKeys { keys, truncated } => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "keys": keys,
                                "truncated": truncated,
                            }))?
                        );
                    } else {
                        for entry in &keys {
                            println!("{:>14}  {}  {}", entry.size, entry.version, entry.key);
                        }
                        if truncated {
                            eprintln!(
                                "(more keys follow; use --start-after {:?})",
                                keys.last().map(|k| k.key.as_str()).unwrap_or("")
                            );
                        }
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Scrub { rate_mib, repair } => {
            use djbod_proto::message::ScrubEvent;
            let mut conn = connect(&cli).await?;
            let id = conn
                .start_scrub(rate_mib.map(|m| m * 1024 * 1024), *repair)
                .await
                .map_err(remote)?;
            let mut findings = 0usize;
            let mut repairs = 0usize;
            let end = loop {
                match conn.next_scrub_event(id).await.map_err(remote)? {
                    Ok(event) => {
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
                                findings += 1;
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
                                findings += 1;
                                println!("cluster check: {finding:?}");
                            }
                            ScrubEvent::Repaired { key, report } => {
                                repairs += 1;
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
            if !cli.json {
                eprintln!("{findings} finding(s), {repairs} repair(s)");
            }
            if let Some(error) = end.error {
                eprintln!("scrub incomplete: {}", describe_detail(&error));
                std::process::exit(2);
            }
            if findings > 0 && !*repair {
                std::process::exit(2);
            }
        }
        Command::MoveShard {
            key,
            shard_index,
            to,
        } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::MoveShard {
                    key: key.clone(),
                    shard_index: *shard_index,
                    target: to.map(djbod_core::record::DeviceId),
                })
                .await
                .map_err(remote)?
            {
                Response::MoveShard {
                    record,
                    source,
                    source_cleaned,
                    rebuilt,
                } => {
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
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Repair { key } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::RepairObject { key: key.clone() })
                .await
                .map_err(remote)?
            {
                Response::RepairObject(report) => {
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
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Cluster { command } => {
            let node = cli
                .node
                .context("no node address: pass --node or set DJBOD_NODE")?;
            let cluster = cli
                .cluster
                .context("no cluster id: pass --cluster or set DJBOD_CLUSTER")?;
            match command {
                ClusterCommand::Show => {
                    let document =
                        djbod_node::membership::fetch_document(&connector(&cli)?, node, cluster)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let reports =
                        djbod_node::membership::fetch_all(&connector(&cli)?, &document).await;
                    if cli.json {
                        let rows: Vec<serde_json::Value> = reports
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "node": r.node,
                                    "address": r.address,
                                    "version": r.result.as_ref().ok().map(|d| d.version),
                                    "error": r.result.as_ref().err(),
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    } else {
                        println!("cluster   {}", document.cluster_id);
                        println!("document  version {} as held by {node}", document.version);
                        println!();
                        println!("{:<36}  {:<21}  VERSION", "NODE", "ADDRESS");
                        for r in &reports {
                            let version = match &r.result {
                                Ok(d) => d.version.to_string(),
                                Err(e) => format!("unreachable: {e}"),
                            };
                            println!("{:<36}  {:<21}  {version}", r.node.0, r.address);
                        }
                    }
                }
                ClusterCommand::SetState { device, state } => {
                    let state = match state {
                        StateArg::Draining => DeviceState::Draining,
                        StateArg::Active => DeviceState::Active,
                    };
                    let (document, changed) = djbod_node::membership::set_device_state(
                        &connector(&cli)?,
                        node,
                        cluster,
                        DeviceId(*device),
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
                            "device {device} is now {state_name} (document version {})",
                            document.version
                        );
                    } else {
                        println!("device {device} was already {state_name}; nothing changed");
                    }
                }
                ClusterCommand::Drain {
                    device,
                    node_id,
                    partial,
                } => {
                    let devices: Vec<Uuid> = match (device, node_id) {
                        (Some(device), _) => vec![*device],
                        (None, Some(node_id)) => {
                            let document = djbod_node::membership::fetch_document(
                                &connector(&cli)?,
                                node,
                                cluster,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                            let found: Vec<Uuid> = document
                                .devices
                                .iter()
                                .filter(|d| {
                                    d.node.0 == *node_id && d.state == DeviceState::Draining
                                })
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
                    let (document, changed) = djbod_node::membership::set_scheme(
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
                    let (document, changed) = djbod_node::membership::set_transport(
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
                    let (document, changed) = djbod_node::membership::set_limits(
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
                        djbod_node::membership::fetch_document(&connector(&cli)?, node, cluster)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let failures = reencode_all(&cli, &document).await?;
                    if failures > 0 {
                        std::process::exit(2);
                    }
                }
                ClusterCommand::RemoveDevice { device } => {
                    let (document, changed) = djbod_node::membership::remove_device(
                        &connector(&cli)?,
                        node,
                        cluster,
                        DeviceId(*device),
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
                    let document = djbod_node::membership::remove_node(
                        &connector(&cli)?,
                        node,
                        cluster,
                        NodeId(*node_id),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "node": node_id,
                                "document_version": document.version,
                            }))?
                        );
                    } else {
                        println!(
                            "node {node_id} removed (document version {}); its process stops on its own, and its devices can be reused with `djbod-node join --wipe-removed-device`",
                            document.version
                        );
                    }
                }
                ClusterCommand::RemoveNode {
                    node_id,
                    force: true,
                    yes,
                } => force_remove_node(&cli, node, cluster, NodeId(*node_id), *yes).await?,
                ClusterCommand::Sync => {
                    let report = djbod_node::membership::sync(&connector(&cli)?, node, cluster)
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
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::GetClusterConfig)
                .await
                .map_err(remote)?
            {
                Response::GetClusterConfig { document } => {
                    println!("{}", serde_json::to_string_pretty(&document)?);
                }
                other => bail!("unexpected response {other:?}"),
            }
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
    let mut conn = connect(cli).await?;
    let mut start_after: Option<String> = None;
    let mut all = Vec::new();
    loop {
        let (keys, truncated) = match conn
            .request(Request::ListKeys(ListQuery {
                prefix: None,
                start_after: start_after.clone(),
                limit: Some(1000),
            }))
            .await
            .map_err(remote)?
        {
            Response::ListKeys { keys, truncated } => (keys, truncated),
            other => bail!("unexpected response {other:?}"),
        };
        start_after = keys.last().map(|e| e.key.clone());
        all.extend(keys.into_iter().map(|e| e.key));
        if !truncated || start_after.is_none() {
            return Ok(all);
        }
    }
}

/// How many objects are stored at a scheme or block size other than the
/// document's.
async fn count_versions_behind(
    cli: &Cli,
    document: &djbod_core::cluster::ClusterDocument,
) -> anyhow::Result<usize> {
    let mut conn = connect(cli).await?;
    let mut behind = 0usize;
    for key in all_keys(cli).await? {
        match conn
            .request(Request::HeadObject { key })
            .await
            .map_err(remote)?
        {
            Response::HeadObject { record } => {
                if !at_current_scheme(&record, document) {
                    behind += 1;
                }
            }
            other => bail!("unexpected response {other:?}"),
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
            let record = match lister
                .request(Request::HeadObject { key: key.clone() })
                .await
            {
                Ok(Response::HeadObject { record }) => record,
                Ok(other) => bail!("unexpected response {other:?}"),
                Err(e) => {
                    failures += 1;
                    println!("FAILED   {key}  head: {}", describe_error(&e));
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
    let mut reader_connection = connect(cli).await?;
    let mut writer_connection = connect(cli).await?;
    let (mut pipe_in, mut pipe_out) = tokio::io::duplex(4 * 1024 * 1024);
    let key = record.key.clone();
    let get = tokio::spawn(async move {
        reader_connection
            .get_object_to_writer(&key, &mut pipe_in)
            .await
    });
    let put = writer_connection
        .put_object_with_metadata(
            &record.key,
            record.size,
            &mut pipe_out,
            DEFAULT_BODY_CHUNK,
            record.content_type.clone(),
            record.user_metadata.clone(),
        )
        .await;
    let got = get.await.context("the read task failed")?;
    match (got, put) {
        (Ok(read), Ok(version)) => {
            if read.version != record.version {
                bail!(
                    "the object changed while being re-encoded (read version {}, expected {}); rerun",
                    read.version,
                    record.version
                );
            }
            Ok(version)
        }
        (Err(e), _) => Err(anyhow::anyhow!("read failed: {}", describe_error(&e))),
        (Ok(_), Err(e)) => Err(anyhow::anyhow!("write failed: {}", describe_error(&e))),
    }
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
    use djbod_node::membership;
    let plan = membership::plan_forced_removal(&connector(cli)?, peer, cluster, node_id)
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
    let document = membership::execute_forced_removal(&connector(cli)?, &plan)
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
    let mut conn = connect(cli).await?;
    let mut failed = 0usize;
    for reference in &plan.affected {
        match conn
            .request(Request::RepairObject {
                key: reference.key.clone(),
            })
            .await
        {
            Ok(Response::RepairObject(report)) => {
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
            Ok(other) => bail!("unexpected response {other:?}"),
            Err(e) => {
                failed += 1;
                println!("LOST     {}  {}", reference.key, describe_error(&e));
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
    let mut conn = connect(cli).await?;
    let id = conn.start_drain(device, partial).await.map_err(remote)?;
    let mut moved = 0usize;
    let mut deleted = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    let end = loop {
        match conn.next_drain_event(id).await.map_err(remote)? {
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
