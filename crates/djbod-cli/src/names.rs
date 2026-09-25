//! Names for the identifiers a person reads (SPEC 6.2.5.1): a device or
//! node by its label where the cluster document gives one, its UUID
//! where it does not. The document is remembered once per command,
//! when the client connects, and nothing here fails: without a document
//! every name is the UUID, so a failure to fetch it never hides the
//! message it would have decorated (issue #123).

use std::sync::OnceLock;

use djbod_core::cluster::{ClusterDocument, NodeId};
use djbod_core::record::DeviceId;

static DOCUMENT: OnceLock<ClusterDocument> = OnceLock::new();

/// Keep this document for the rest of the command. The first one wins;
/// a command that connects twice names things the same way throughout.
pub(super) fn remember(document: ClusterDocument) {
    let _ = DOCUMENT.set(document);
}

fn device_label(id: DeviceId) -> Option<&'static str> {
    DOCUMENT
        .get()
        .and_then(|d| d.device(id))
        .and_then(|d| d.label.as_deref())
}

fn node_label(id: NodeId) -> Option<&'static str> {
    DOCUMENT
        .get()
        .and_then(|d| d.node(id))
        .and_then(|n| n.label.as_deref())
}

/// A device by its label, else its UUID: for the compact places, a
/// table cell or a per-item line.
pub(super) fn device(id: DeviceId) -> String {
    device_label(id)
        .map(str::to_string)
        .unwrap_or_else(|| id.0.to_string())
}

/// A node by its label, else its UUID.
pub(super) fn node(id: NodeId) -> String {
    node_label(id)
        .map(str::to_string)
        .unwrap_or_else(|| id.0.to_string())
}

/// A device's full identity, label with the UUID in brackets, for the
/// lines that matter: what a command acted on, what an error names.
pub(super) fn device_identity(id: DeviceId) -> String {
    match device_label(id) {
        Some(label) => format!("{label} ({})", id.0),
        None => id.0.to_string(),
    }
}

/// A node's full identity, as `device_identity`.
pub(super) fn node_identity(id: NodeId) -> String {
    match node_label(id) {
        Some(label) => format!("{label} ({})", id.0),
        None => id.0.to_string(),
    }
}

/// Where a shard is, as a person reads it: "node devbox4 device
/// devbox4-d0", each by its label or, without one, its UUID. A device
/// the document does not list has no node to name.
pub(super) fn device_and_node(id: DeviceId) -> String {
    match DOCUMENT.get().and_then(|d| d.device(id)) {
        Some(entry) => format!("node {} device {}", node(entry.node), device(id)),
        None => format!("device {}", id.0),
    }
}
