//! The node process (SPEC 4, 5, 6, 19).
//!
//! Every node runs this same program. This crate contains:
//!
//! - [`config`]: the per-node TOML configuration file (6.1).
//! - [`node`]: opening a node's devices and cluster document, and creating
//!   a new cluster from one node.
//! - [`wire`]: reading and writing protocol frames over tokio streams.
//! - [`client`]: a connection to a node, used by the coordinator, the
//!   command-line tool, and the tests.
//! - [`server`]: accepting connections, the `Hello` exchange, and
//!   dispatching requests.
//! - [`local_ops`]: the node-to-node operations (19.1.3), served from this
//!   node's own devices.
//!
//! Disk work runs on tokio's blocking pool via `spawn_blocking`;
//! `djbod-core` stays synchronous (4.4).

pub mod client;
pub mod config;
pub mod local_ops;
pub mod node;
pub mod server;
pub mod wire;
