# Commands

This is the reference for the four binaries and `scripts/djbod-pki.sh`.
The procedures that combine them are in [Scenarios](scenarios.md).
Flags shown on a subcommand are also accepted in front of it. Build
`1b8e434` (version 0.2.5) is the tree these descriptions were checked
against. `--help` on the binary you installed is the check when the two
disagree.

## `djbod`

Client and administration tool.

```text
djbod [--node ADDR[,ADDR...]] [--cluster UUID] [--json]
     [--tls-ca FILE] [--tls-cert FILE] [--tls-key FILE]
     <command>
```

| Flag | Environment | Meaning |
|---|---|---|
| `--node` | `DJBOD_NODE` | One or more `ip:port` addresses, comma-separated. Tried in order. The first that answers is the coordinator for the request: it encodes, fans the work out, and checks checksums. A later failure moves to the next address in this list only. [Concepts](concepts.md#which-node-coordinates) is why the order matters on machines that differ in CPU, link speed, or region. |
| `--cluster` | `DJBOD_CLUSTER` | Cluster id from `init-cluster`. Required for every command except `identity` and `get-cluster-id`. |
| `--json` | | One JSON value, or one JSON object per event for streaming commands. |
| `--tls-ca` | `DJBOD_TLS_CA` | PEM authority or bundle. Alone, the connection is encrypted and presents no certificate. |
| `--tls-cert` | `DJBOD_TLS_CERT` | Client certificate. Requires `--tls-key` and `--tls-ca`. |
| `--tls-key` | `DJBOD_TLS_KEY` | Client private key. Must be readable only by its owner. |

A flag wins over the environment variable. Exit codes for the whole
tool are in [Day to day](day-to-day.md#exit-codes).

### `status`

Every device's state, free space, label, owning node, and that node's
build. Also the cluster name and id, document version, which node
answered (`answered by node`, the coordinator), the build, and the
transport. A device the node cannot read is `<state>, unavailable`, and
standard error says `1 device is unavailable` or `N devices are
unavailable`, plus `node <uuid> unreachable:` for each node that did
not answer. The command still exits 0. A removed device stays in the
table as `removed`, with no free space and without the `unavailable`
suffix. Fails with `DocumentVersionMismatch` when a node that answered
holds a different document version.

### `contents [DEVICE...] [--node-id NODE]`

Versions, keys, blocks, and shard bytes on each device, counted from
records without reading payloads. No arguments means every device that
is not `removed`. A device name is a UUID or a label. `--node-id`
limits the list to one node's devices. A device its own node cannot
read is named on standard error and the others are still printed. A
node that does not answer fails the command with `NodeUnreachable`. A
device with zero versions is called out as empty.

### `get-cluster-id`

Prints the cluster UUID of the node at `--node`. Does not take
`--cluster`. `--json` adds the name, the node, and the build. The plain
form is what `export DJBOD_CLUSTER=$(djbod get-cluster-id)` expects.

### `identity`

Who is at `--node`: cluster name and id, node label and id, address,
build, document version, transport. Does not take `--cluster`.

### `put <KEY> <FILE> [--content-type TYPE]`

Store a file under a key. `FILE` of `-` reads standard input, all of it,
because the size is sent first. Prints `stored <key> as version <id>`.
Exit 2 when the object was stored and one or more devices were skipped
as unavailable (`placed around 1 unavailable device`, or `devices`).
Exit 1 when nothing was stored (`InsufficientDevices`,
`NodeUnreachable` when any node is silent, `ObjectTooLarge`,
`KeyTooLong`, and the other refusals).

### `get <KEY> [FILE]`

Fetch an object. `FILE` defaults to `-`, which is standard output. A
failed fetch to a named file removes the partial file. Exit 2 when the
bytes are correct and blocks were reconstructed, or when a record copy
could not be read and at least `k` copies agreed. The damage stays on
disk until `repair`. A node that is down does not by itself fail the
read; fewer than `k` readable copies, or more than `m` unreadable
shards, does.

### `head <KEY>`

The record: key, version, size, created time, checksum, scheme and
block size, content type if any, and `shard <index> device <uuid>` for
each shard. Exit 1 with `NotFound` when the key is absent. Exit 2 when
the record was trusted and a copy could not be read. Exit 1 with
`RecordsInconsistent` when the copies that were read disagree, or fewer
than `k` of them can be read and the cause is not an unavailable device.

### `delete <KEY>`

Delete the current version. Prints `deleted <key>`. A missing key is
`NotFound`, exit 1.

### `list [--prefix PREFIX] [--start-after KEY] [--limit N]`

Keys in order. Each line is the size, the version id, and the key.
`--limit` caps the page. A page also stops at 8 MiB of key text.
Truncation is reported on standard error with the `--start-after` value
to continue from. Devices that could not be read are named. Exit 2 when
`k + m` or more are out, because a key stored only there may be missing
from the page. Exit 0 when fewer are out; the line then says every key
is still listed, which holds for objects written at the document's
current scheme. `--json` includes `truncated`, `unread`, and `complete`.

### `cluster-config`

The cluster document as JSON: version, id, name, `k`, `m`, block size,
headroom, the three size limits, transport, nodes, devices.

### `repair <KEY>`

Rebuild damaged or missing shards of one object from the intact ones,
and rewrite a record copy that is missing once a bad record file is out
of the way. Prints each shard's condition and whether it was rewritten
or rebuilt onto another device, then `N shards rewritten`. Fails with
`NodeUnreachable` if any node does not answer. Fails with
`DeviceUnavailable` while a disk that holds a shard is still a member
and cannot be read: the replacement would be written back to that disk.
Fails with `BlockChecksumMismatch` when a stripe has fewer than `k`
usable blocks, and changes nothing in that case. See
[Scenarios](scenarios.md#a-bad-block-or-a-missing-shard-file).

### `move-shard <KEY> <SHARD_INDEX> [--to DEVICE]`

Copy one shard to another device and publish a new record revision.
`--to` is a device UUID or label. Without it, the destination is chosen
like a write. Prints whether the shard was copied or rebuilt, and warns
when the old copy could not be deleted.

### `scrub [--rate-mib N] [--repair]`

Every node checks its own disks, then the cross-node checks run.
`--rate-mib` caps each node's read rate. `--repair` rebuilds objects
the checks found damaged. It does not rebuild a device that is
unavailable and still a member. A node that does not answer is named,
the cross-node checks stop, and the exit code is 3 or 4. Exit codes are
0, 2, 3, and 4, as in [Day to day](day-to-day.md#exit-codes). The human
output ends with the outcome, then, when a device was unchecked, how
many versions have a shard on it. `--json` is one event per line and
the exit code is the only summary.

### `cluster`

Membership and settings. Every subcommand talks to the cluster and
needs `--node` and `--cluster`. Devices and nodes are named by UUID or
label.

| Subcommand | What it does |
|---|---|
| `show` | Asks every node for its document. Prints the cluster, the version held by the node you asked, and a table of node, label, address, build, and version. An unreachable node is a row, not a failure. |
| `sync` | Brings every reachable node up to the highest document version any of them holds. Prints `updated`, `already current`, and `unreachable`. Exit 2 if any node was unreachable. |
| `set-state <DEVICE> <draining\|active>` | Changes the device's state. Moves no data. `draining` stops new shards landing on it. |
| `set-label <DEVICE> [LABEL] [--clear]` | Label, 1 to 128 bytes, no whitespace, not a UUID, unique in the cluster. `--clear` removes it. |
| `set-name [NAME] [--clear]` | Cluster display name, 1 to 128 bytes. Spaces are allowed, so quote it. The id clients send does not change. |
| `set-node-label <NODE> [LABEL] [--clear]` | Same rules as a device label, for a node. |
| `set-address <NODE> <IP:PORT[,IP:PORT...]> ` | Replace the addresses in the document. The node must still answer at its current address. The first address is the one used. A node that has already moved updates the document from its configuration at startup instead. |
| `drain [DEVICE] [--node-id NODE] [--partial]` | One pass over a device that is already `draining`. Prints an estimate, then `moved`, `skipped`, or `deleted` per version. Exit 2 if the estimate says the remaining active devices are fewer than `k + m` or lack the free space, unless `--partial`, and exit 2 if any version was skipped. `--node-id` repeats the pass for every draining device of that node. |
| `set-scheme --k K --m M [--block-size BYTES]` | Changes the scheme for new writes. Moves nothing. `k` is 1 to 32, `m` is 0 to 8, and `k + m` is at most 64, so `1+0` is legal. Refuses when fewer devices are active than the new `k + m`. Reports how many existing objects are on another scheme (`1 object is stored` or `N objects are stored`). Block size, if given, is a multiple of 4096 from 64 KiB to 64 MiB. A larger block is a larger stripe allocation on the coordinator, bounded by the 64 MiB frame cap. [Concepts](concepts.md#block-size). |
| `reencode` | Rewrites every object still on another scheme or block size, one at a time, through this client. Safe to interrupt and rerun. Exit 2 if any object failed. |
| `set-transport <plain\|tls-optional\|tls>` | Changes how connections are made. Leaving `plain` is refused while any node has no TLS material, and the error names the node. See [TLS](tls.md). |
| `set-limits [--max-key-bytes N] [--max-object-bytes N] [--max-user-metadata-bytes N]` | At least one flag. Applies to new writes. Prints all three values. |
| `remove-device <DEVICE> [--force] [--yes]` | Without `--force`, the device must be `draining` and must hold no current shard. With `--force`, mark it `removed` without reading it and without moving data. `--force` describes the consequence and asks you to type the device id. `--yes` skips that prompt and requires `--force`. Rebuild afterwards with `scrub --repair`. |
| `remove-node <NODE> [--force] [--yes]` | Without `--force`, every device of the node must be `draining` or `removed` and must hold no current shard. The node acknowledges, then exits. Refuses to remove the only node. With `--force`, the node must not answer. The command counts versions that have shards there, names any with more than `m` shards on that node, asks you to type the node id, removes the node without its acknowledgement, and rebuilds the rest. Exit 2 if a rebuild fails and the object is lost. `--yes` skips the prompt. |

## `djbod-node`

The node process. `--log-format text` (the default) or `json` applies to
every subcommand. Logs go to standard error. `RUST_LOG` selects the
level. The default level is `info`.

Subcommands take the [configuration](setup.md#configuration-reference)
as `--config` or as individual flags. `init-cluster`, `join`,
`add-device`, `run`, and `scrub` all accept that set.

### `init-cluster`

Create a cluster of this node and its devices. Initialises every
configured directory. Prints the cluster id, the node id, and each
device id and path.

| Flag | Default | Meaning |
|---|---|---|
| `--name` | none | Display name. `DJBOD_CLUSTER_NAME`. |
| `--k` | 3 | Data shards. |
| `--m` | 1 | Parity shards. |
| `--block-size` | 1048576 | Bytes, multiple of 4096, 64 KiB to 64 MiB. The coordinator's stripe buffer and encoded blocks scale with this, up to the frame cap. [Concepts](concepts.md#block-size). |
| `--headroom` | 0.05 | Fraction of each filesystem kept free, from 0 to 0.5. Cannot be changed later. [Concepts](concepts.md#headroom). |
| `--max-key-bytes` | 16384 | |
| `--max-object-bytes` | 1099511627776 | 1 TiB. |
| `--max-user-metadata-bytes` | 10485760 | 10 MiB. |
| `--wipe-removed-device` | off | Erase a directory that already belonged to a cluster. Destroys its data. |

A non-empty device directory is refused, naming what was found. Two
devices on one filesystem are refused unless `allow_shared_filesystem`
is set, in which case a warning is logged. Fewer active devices than
`k + m` is allowed and warned about. Every later `put` fails until the
count is high enough.

### `join --peer ADDR --cluster UUID`

Fetch the document from a peer, initialise this node's devices, add this
node to the document. Prints the node id, the document version, and the
new device ids. Does not write `node.toml` and does not set
`bootstrap_peers`. Every node already in the document has to be
running; otherwise the proposal fails with `cannot reach`. Start the
node afterwards with `run` and the same config, and start it before the
next machine joins. `--wipe-removed-device` erases devices that were
removed from this cluster before. A peer in a different cluster is
refused.

### `add-device --path PATH [--path PATH...] [--peer ADDR]`

`PATH` must already be listed in the configuration and must be an empty
directory, unless `--wipe-removed-device` is set. Talks to `--peer`, or
to this node's own advertised address when `--peer` is omitted. Prints
the new document version and tells you to restart the node. The running
process does not open the new path until that restart.

### `run`

Adopt a newer document from `bootstrap_peers`, and if `listen` or
`advertise` disagrees with the document, propose the configured address.
Refuse to start if that proposal cannot be accepted. Then serve. A node
removed by the cluster logs that fact and exits. A missing device
directory, or an empty mount point, is logged and the device is
unavailable. The node still serves its other devices. A directory with
an `objects` tree and no identity file refuses startup.

### `scrub [--rate-mib N] [--json]`

Offline check of the devices in the configuration. Verifies records and
shard blocks against their checksums. Prints findings and a per-device
summary. Exit 2 when anything was wrong. Does not repair and does not
contact other nodes. `--device` replaces the configured device list for
this run, which is how you limit it to some paths. Safe to run while
the node is also running. The cluster-wide scrub that can repair is
`djbod scrub`.

## `djbod-recover`

Reads device directories with no node running. Never writes to a device.
Does not need `DISTRIBUTED-JBOD-DEVICE.json`.

### `list <DEVICE_PATH...>`

Every key and version found, with revision, size, shards present, and
whether `k` intact shards were found. Exit 2 when any version is short
of `k` shards or a file could not be read. A shard with no record is
listed under its key hash.

### `extract <KEY> --out FILE [--version ID] <DEVICE_PATH...>`

Reassemble one object from any `k` intact shards on the given paths.
The newest version is used unless `--version` is set. Refuses to
overwrite `FILE`. Verifies blocks and the whole-object checksum. Exit 1
when fewer than `k` intact shards exist.

## `djbod-ui`

```text
djbod-ui --bootstrap-node ADDR[,ADDR...] --cluster UUID [--listen 127.0.0.1:5264]
         [--host NAME] [--tls-ca FILE] [--tls-cert FILE] [--tls-key FILE]
```

Serves the administration page. `--bootstrap-node`
(`DJBOD_BOOTSTRAP_NODE`) is the ordered list of nodes to try. The first
that answers coordinates the page. After the cluster document has been
read, the other addresses in it are tried too. `--node` and `DJBOD_NODE`
are the deprecated spelling: accepted with a warning, and ignored when
the new spelling is set. `--cluster` and the TLS flags match `djbod`.
`--listen` defaults to `127.0.0.1:5264`. There is no authentication on
the HTTP port. `--host` adds a DNS name the server will answer, besides
IP addresses and `localhost`. Other `Host` values get HTTP 403. The page
and the `/api/` routes are described in [Day to day](day-to-day.md#the-web-ui).

## `scripts/djbod-pki.sh`

```text
djbod-pki.sh [--dir DIR] [--days N] init-ca [--name NAME]
djbod-pki.sh [--dir DIR] [--days N] node <name> <ip>[,<ip>...]
djbod-pki.sh [--dir DIR] [--days N] client <name>
djbod-pki.sh [--dir DIR] list
```

`--dir` defaults to `$DJBOD_PKI_DIR` or `./djbod-pki`. `--days` defaults
to 3650. `init-ca` writes `ca.crt` and `ca.key`. `node` writes
`<name>.crt` and `<name>.key` with an IP subject alternative name and
extended key usage for both server and client. `client` writes a
certificate with client usage only. Keys are mode 0600. Existing files
are never overwritten. `list` prints subject, expiry, and subject
alternative names. The node does not call this script. See [TLS](tls.md).

## Errors you will see

The node returns a code and a message. `djbod` prints both, then any of
node, device, key, version, shard, and stripe that the node supplied.

| Code | When it appears | What to do |
|---|---|---|
| `NodeUnreachable` | A node in the document did not answer, on a command that requires every node: `put`, `delete`, `repair`, `contents`, and membership changes. `status`, `cluster show`, and a `get` that still has `k` copies do not use this as a failure. | `cluster show`. Bring the node back, or [force-remove it](scenarios.md#a-node-will-never-come-back). |
| `DeviceUnavailable` | A device could not be opened or its identity file could not be read. `repair` returns this while the disk is still a member. | [Retire it or mount it](scenarios.md#a-disk-is-unreadable). |
| `BlockChecksumMismatch` | A block failed its checksum, and not enough intact blocks remained to rebuild. The message counts `damaged blocks` and says how many usable blocks the stripe had. | `repair` when `m` covers it. Otherwise the object is lost. |
| `RecordsInconsistent` | The record copies that were read disagree, fewer than `k` can be read, or a record file is not valid. A missing copy that still leaves `k` agreeing copies is exit 2 on `get` and `head`, not this code. | [Damaged record](scenarios.md#a-record-file-is-unreadable). |
| `NotFound` | No object under that key on the devices that could be read, and fewer than `k + m` devices are out. A `1+0` object on a device that is out can be reported this way after the document scheme has been raised. | `status`, then `reencode` once the device is back, if the object was an older narrower scheme. |
| `InsufficientDevices` | Fewer than `k + m` devices can take a shard, or a rebuild has nowhere to go. The message names unavailable devices when those are why. | Add a device or a node before draining or force-removing. |
| `DocumentVersionMismatch` | A node that answered holds a different document version. `status` fails. `cluster show` still prints both versions. | `cluster sync`. `bootstrap_peers` is what avoids this at the next start. |
| `ObjectTooLarge`, `KeyTooLong`, `MetadataTooLarge` | The write exceeds a limit in the document. | `cluster set-limits`, or send a smaller object. |
| `TlsRequired` | The transport is `tls` and the connection was plain or had no client certificate. | [TLS](tls.md). |
