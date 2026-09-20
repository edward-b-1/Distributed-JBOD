//! The client side of Distributed-JBOD (SPEC 19, Appendix C.2): what a
//! program needs to talk to a node and nothing of the node itself.
//!
//! - [`wire`]: reading and writing protocol frames over tokio streams.
//! - [`transport`]: the byte stream beneath the frames, plain TCP or TLS
//!   with a client configuration (19.1.6).
//! - [`connection`]: one connection to a node: the `Hello` exchange,
//!   requests and responses, and the streaming operations, including the
//!   node-to-node shard transfers the coordinator uses.
//! - [`client`]: what a program uses (SPEC 20.8): several node addresses
//!   with failover, the cluster id learned when not given, and one method
//!   per operation. [`blocking`] is the same without `async`.
//!
//! The node, the command-line tool, and the web UI are all built on this
//! crate; so is any other client.

/// This software's build, crate version and git commit, sent in every
/// `Hello` and printed by `--version`. The commit comes from `build.rs`.
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("DJBOD_GIT_COMMIT"));

pub mod blocking;
pub mod client;
pub mod connection;
pub mod transport;
pub mod wire;

pub use client::{Client, ClientOptions, Error, Identity, ListPage, Status};
