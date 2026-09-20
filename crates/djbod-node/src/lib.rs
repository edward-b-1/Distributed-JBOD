//! The node process (SPEC 4, 5, 6, 19).
//!
//! Every node runs this same program. This crate contains:
//!
//! - [`config`]: the per-node TOML configuration file (6.1).
//! - [`node`]: opening a node's devices and cluster document, and creating
//!   a new cluster from one node.
//! - [`server`]: accepting connections, the `Hello` exchange, and
//!   dispatching requests.
//! - [`local_ops`]: the node-to-node operations (19.1.3), served from this
//!   node's own devices.
//! - [`coordinator`]: the client-facing operations (19.1.3), served by
//!   fanning out node-to-node operations to every node in the cluster
//!   document, including this one (4.1).
//! - [`ulid`]: version identifier generation (9.2.3).
//! - [`membership`]: changing the cluster document without a master,
//!   joining a node, syncing stragglers, and startup adoption (6.2.6,
//!   18.1).
//!
//! Frames and connections come from the `djbod-client` crate, which the
//! coordinator uses to reach holders and every client program uses too.
//!
//! Disk work runs on tokio's blocking pool via `spawn_blocking`;
//! `djbod-core` stays synchronous (4.4).

pub mod config;
pub mod coordinator;
pub mod local_ops;
pub mod membership;
pub mod node;
pub mod server;
pub mod transport;
pub mod ulid;
