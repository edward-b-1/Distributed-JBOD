//! Human-readable tables. JSON output is serialized by the command handlers.

use comfy_table::{presets::NOTHING, CellAlignment, Table};
use djbod_client::admin::NodeDocument;
use djbod_core::cluster::ClusterDocument;
use djbod_proto::message::{DeviceContents, DeviceStatus, KeyEntry};

use super::human_bytes;

/// Columns two spaces apart with no border, each as wide as its widest
/// cell, measured in terminal columns so wide characters line up.
/// `right_aligned` names the numeric columns, which align on their right
/// edge. Lines carry no trailing spaces.
fn render(mut table: Table, right_aligned: &[usize]) -> String {
    table.load_style(NOTHING);
    let last = table.column_count().saturating_sub(1);
    for (i, column) in table.column_iter_mut().enumerate() {
        column.set_padding((0, if i == last { 0 } else { 2 }));
        if right_aligned.contains(&i) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    let mut out = String::new();
    for line in table.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub(super) fn contents(document: &ClusterDocument, rows: &[DeviceContents]) -> String {
    let mut table = Table::new();
    table.set_header([
        "DEVICE",
        "LABEL",
        "NODE LABEL",
        "STATE",
        "VERSIONS",
        "KEYS",
        "BLOCKS",
        "SHARD BYTES",
    ]);
    for c in rows {
        table.add_row([
            c.device.0.to_string(),
            document
                .device(c.device)
                .and_then(|d| d.label.as_deref())
                .unwrap_or("-")
                .to_string(),
            document
                .node(c.node)
                .and_then(|n| n.label.as_deref())
                .unwrap_or("-")
                .to_string(),
            format!("{:?}", c.state).to_lowercase(),
            c.versions.to_string(),
            c.keys.to_string(),
            c.blocks.to_string(),
            human_bytes(c.shard_bytes),
        ]);
    }
    render(table, &[4, 5, 6, 7])
}

pub(super) fn status(devices: &[DeviceStatus]) -> String {
    let mut table = Table::new();
    table.set_header([
        "DEVICE",
        "LABEL",
        "NODE",
        "NODE LABEL",
        "STATE",
        "TOTAL",
        "FREE",
    ]);
    for d in devices {
        table.add_row([
            d.device.0.to_string(),
            d.label.as_deref().unwrap_or("-").to_string(),
            d.node.0.to_string(),
            d.node_label.as_deref().unwrap_or("-").to_string(),
            format!("{:?}", d.state).to_lowercase(),
            human_bytes(d.total_bytes),
            human_bytes(d.free_bytes),
        ]);
    }
    render(table, &[5, 6])
}

pub(super) fn cluster_show(document: &ClusterDocument, reports: &[NodeDocument]) -> String {
    let mut table = Table::new();
    table.set_header(["NODE", "LABEL", "ADDRESS", "BUILD", "VERSION"]);
    for r in reports {
        let version = match &r.result {
            Ok(d) => d.version.to_string(),
            Err(e) => format!("unreachable: {e}"),
        };
        // Every listed address; the first is the one used.
        let addresses = document
            .node(r.node)
            .map(|n| n.addresses.join(", "))
            .unwrap_or_else(|| r.address.clone());
        // No build from a node that could not be reached.
        let build = r.build.as_deref().unwrap_or("-");
        table.add_row([
            r.node.0.to_string(),
            r.label.as_deref().unwrap_or("-").to_string(),
            addresses,
            build.to_string(),
            version,
        ]);
    }
    render(table, &[])
}

pub(super) fn list(keys: &[KeyEntry]) -> String {
    let mut table = Table::new();
    for entry in keys {
        table.add_row([
            entry.size.to_string(),
            entry.version.to_string(),
            entry.key.clone(),
        ]);
    }
    render(table, &[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use djbod_core::cluster::{DeviceState, NodeEntry, NodeId};
    use djbod_core::record::DeviceId;
    use djbod_core::version::VersionId;
    use uuid::Uuid;

    fn empty_document() -> ClusterDocument {
        serde_json::from_value(serde_json::json!({
            "version": 1, "cluster_id": Uuid::nil(), "k": 1, "m": 1,
            "block_size": 65536, "independence_level": "device", "headroom": 0.0,
            "max_key_bytes": 16384, "max_object_bytes": 1099511627776u64,
            "max_user_metadata_bytes": 10485760, "transport": "plain",
            "nodes": [], "devices": [],
        }))
        .expect("document")
    }

    #[test]
    fn list_aligns_full_size_range_and_preserves_keys() {
        let version = VersionId([1; 16]);
        let keys = [
            KeyEntry {
                key: "short".to_string(),
                size: 1,
                version,
            },
            KeyEntry {
                key: "long/界".repeat(20),
                size: u64::MAX,
                version,
            },
        ];
        let out = list(&keys);
        for (line, entry) in out.lines().zip(&keys) {
            assert_eq!(
                line,
                format!("{:>20}  {version}  {}", entry.size, entry.key)
            );
        }
        assert_eq!(out.lines().count(), 2);
        assert_eq!(list(&[]), "");
    }

    #[test]
    fn contents_aligns_counts_exceeding_nine_digits() {
        let document = empty_document();
        let rows: Vec<_> = [1, u64::MAX]
            .into_iter()
            .map(|count| DeviceContents {
                device: DeviceId(Uuid::nil()),
                node: NodeId(Uuid::nil()),
                state: DeviceState::Active,
                versions: count,
                keys: count,
                blocks: count,
                shard_bytes: 0,
            })
            .collect();
        let out = contents(&document, &rows);
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        for (line, row) in lines.zip(&rows) {
            for name in ["VERSIONS", "KEYS", "BLOCKS"] {
                let end = header.find(name).unwrap() + name.len();
                assert_eq!(
                    line[end - 20..end].trim(),
                    row.versions.to_string(),
                    "{out}"
                );
                assert_eq!(&line[end..end + 2], "  ", "{out}");
            }
        }
    }

    #[test]
    fn cluster_show_aligns_addresses_builds_and_unreachable_nodes() {
        let mut document = empty_document();
        let long_build = "0.1.0+0123456789abcdef0123456789abcdef01234567";
        let mut reports = Vec::new();
        for i in 0..3 {
            let id = NodeId(Uuid::from_u128(i));
            let label = if i == 1 { Some("n".repeat(128)) } else { None };
            let addresses = if i == 1 {
                vec![
                    "[2001:db8:abcd:1234:5678:abcd:1234:5678]:5263".to_string(),
                    "127.0.0.1:5263".to_string(),
                ]
            } else {
                vec![format!("127.0.0.1:{}", 5264 + i)]
            };
            document.nodes.push(NodeEntry {
                id,
                label: label.clone(),
                addresses: addresses.clone(),
            });
            reports.push(NodeDocument {
                node: id,
                label,
                address: addresses[0].clone(),
                build: match i {
                    1 => Some(long_build.to_string()),
                    2 => None,
                    _ => Some("0.1.0+abc".to_string()),
                },
                result: if i == 2 {
                    Err("connection refused".to_string())
                } else {
                    Ok(empty_document())
                },
            });
        }
        let out = cluster_show(&document, &reports);
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        let address_start = header.find("ADDRESS").unwrap();
        let build_start = header.find("BUILD").unwrap();
        let version_start = header.find("VERSION").unwrap();
        let builds = ["0.1.0+abc", long_build, "-"];
        let versions = ["1", "1", "unreachable: connection refused"];
        for (i, line) in lines.enumerate() {
            assert_eq!(
                line[address_start..build_start].trim(),
                document.nodes[i].addresses.join(", "),
                "{out}"
            );
            assert_eq!(line[build_start..version_start].trim(), builds[i], "{out}");
            assert_eq!(&line[version_start..], versions[i], "{out}");
        }
    }
}
