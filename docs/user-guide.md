# Distributed-JBOD user guide

This guide covers the implementation at commit `d9601b7` (21 September
2026). It takes an operator from an empty installation through normal use,
maintenance, and recovery. The shorter [getting-started walkthrough](getting-started.md)
is useful for an initial experiment; [SPEC.md](../SPEC.md) records the
format and design decisions.

## Contents

- [What to expect](#what-to-expect)
- [Install and try it locally](#install-and-try-it-locally)
- [Deploy across machines](#deploy-across-machines)
- [Configuration reference](#configuration-reference)
- [Store and retrieve objects](#store-and-retrieve-objects)
- [Use the web interface](#use-the-web-interface)
- [Enable TLS](#enable-tls)
- [Inspect and maintain the cluster](#inspect-and-maintain-the-cluster)
- [Grow and shrink storage](#grow-and-shrink-storage)
- [Change the coding scheme and limits](#change-the-coding-scheme-and-limits)
- [Handle failures and recover data](#handle-failures-and-recover-data)
- [Use the client libraries](#use-the-client-libraries)
- [Troubleshooting](#troubleshooting)

## What to expect

Distributed-JBOD stores named objects on directories distributed across
machines. A key such as `photos/cat.jpg` names a complete object; the slash
is part of the key, not a filesystem directory. The current implementation
has one namespace, `default`, and uses its own TCP protocol. It does not
provide an S3 endpoint, a mounted filesystem, range reads, or multipart
uploads. PUT replaces a key; internal version identifiers do not provide
user-visible version history or an undelete facility.

Each object is split into `k` data shards and `m` parity shards on `k+m`
different devices. With sufficient intact metadata, any `k` intact shards
can reconstruct it. For example, `3+1` tolerates the loss of one shard;
`4+2` tolerates two. This is a **recoverability** property. Ordinary reads
stop on missing or damaged required data, and repair is an explicit
operation. Metadata lookups require every listed node to answer, so a
stopped node can prevent reads even of objects stored on other machines.

Placement separates devices, not hosts, racks, or power supplies. Losing
one host can lose several shards of the same object. Four directories on
one physical disk provide no physical redundancy. Keep independent
backups of important objects: parity does not protect against an accepted
delete, an unwanted overwrite, or losses beyond the chosen scheme.

The project is young. Its [status](../README.md#status) and the limits above
should inform where you use it.

## Install and try it locally

### Prerequisites and build

Use Linux, a Rust toolchain with Cargo, and a working native compiler and
linker. The storage filesystem must support native `fallocate`; ext4 is
the tested deployment filesystem. Device directories must be writable by
the account running the node. OpenSSL 3 is needed for the TLS helper later.

Obtain a checkout, or enter one you already have:

```sh
git clone https://github.com/edward-b-1/Distributed-JBOD.git
cd Distributed-JBOD
```

Build the four standalone tools:

```sh
cargo build --locked --release -p djbod-node -p djbod-cli -p djbod-ui -p djbod-recover
export PATH="$PWD/target/release:$PATH"
djbod-node --version
djbod --help
```

The binaries are `djbod-node` (server and local setup), `djbod` (client and
administration), `djbod-ui` (browser interface), and `djbod-recover`
(offline extraction). Keep the checkout, commit ID, and binaries used for
an installation together. The commands below assume these binaries are
on `PATH` in each terminal.

### Create a disposable cluster

Choose an unused directory. This example uses `/tmp/djbod-guide`; its
contents are disposable and may disappear on reboot. Each new device
directory must be empty.

```sh
mkdir -p /tmp/djbod-guide/state /tmp/djbod-guide/d0 /tmp/djbod-guide/d1 /tmp/djbod-guide/d2 /tmp/djbod-guide/d3
cat > /tmp/djbod-guide/node.toml <<'TOML'
node_id = "6a2d5c1e-3f0b-4b1a-9d2e-0c7e8a9b1f22"
listen = "127.0.0.1:5263"
state_dir = "/tmp/djbod-guide/state"
devices = ["/tmp/djbod-guide/d0", "/tmp/djbod-guide/d1", "/tmp/djbod-guide/d2", "/tmp/djbod-guide/d3"]
allow_shared_filesystem = true
TOML
djbod-node init-cluster --config /tmp/djbod-guide/node.toml --name guide-demo --k 3 --m 1
djbod-node run --config /tmp/djbod-guide/node.toml
```

The last command remains in the foreground. The shared-filesystem warning
is expected for this experiment. `init-cluster` creates device identities
and the cluster document, then prints their UUIDs. It is a one-time setup
command; use `run` for subsequent starts.

In another terminal, with the binaries on `PATH`:

```sh
export DJBOD_NODE=127.0.0.1:5263
export DJBOD_CLUSTER=$(djbod get-cluster-id)
printf 'Hello from Distributed-JBOD\n' > /tmp/djbod-guide/hello.txt
djbod put notes/hello.txt /tmp/djbod-guide/hello.txt --content-type text/plain
djbod list --prefix notes/
djbod head notes/hello.txt
djbod get notes/hello.txt /tmp/djbod-guide/copy.txt
cmp /tmp/djbod-guide/hello.txt /tmp/djbod-guide/copy.txt
djbod status
```

`cmp` exits successfully without output when the contents agree. Stop the
foreground node with Ctrl-C. Restart using the same configuration and
identities; do not initialise it again. Delete the experiment's directory
only after stopping every process using it and deciding its data is no
longer needed.

## Deploy across machines

### Prepare disks and identities

Use one node process per machine, with a persistent UUID and a separate
state directory. Mount each independent storage device before starting
the node, create an empty directory on it, and grant the service account
access. Do not point the configuration at an unmounted mountpoint: the
underlying system disk is not the intended device.

Keep `allow_shared_filesystem` false. The node rejects configured devices
sharing a filesystem, but that check cannot establish independence of
partitions, controllers, enclosures, or power supplies. Plan those physical
dependencies yourself.

Use stable, reachable IP addresses. TCP port 5263 is the default. Peers
must reach every node's advertised address. The following is an example
for `nas1` at `10.0.0.1`, with three separately mounted disks:

```toml
# /etc/djbod/node.toml
node_id = "5f0c1d2e-8a9b-4c3d-9e1f-2a3b4c5d6e7f"
listen = "0.0.0.0:5263"
advertise = "10.0.0.1:5263"
state_dir = "/var/lib/djbod"
devices = ["/mnt/disk0/djbod", "/mnt/disk1/djbod", "/mnt/disk2/djbod"]
bootstrap_peers = ["10.0.0.2:5263", "10.0.0.3:5263"]
```

Give `nas2` and `nas3` their own UUIDs, advertised addresses, and peer lists.
Generate UUIDs with `uuidgen` or read `/proc/sys/kernel/random/uuid`. Local
directory names may be the same on different machines; state and device
contents must belong to their respective nodes.

### Initialise, join, and start

On `nas1`:

```sh
djbod-node init-cluster --config /etc/djbod/node.toml --name home-nas --k 4 --m 2
djbod-node run --config /etc/djbod/node.toml
```

With only three devices, this can initialise but cannot yet accept a
`4+2` write. On each additional machine, set `DJBOD_CLUSTER` to the UUID
printed above and run these commands, one machine at a time:

```sh
djbod-node join --config /etc/djbod/node.toml --peer 10.0.0.1:5263 --cluster "$DJBOD_CLUSTER"
djbod-node run --config /etc/djbod/node.toml
```

Start each joined node before joining the next: the current members must
acknowledge the next membership change. Once all three machines run,
there are nine devices and enough candidates for a six-shard write.

On an administration machine:

```sh
export DJBOD_NODE=10.0.0.1:5263,10.0.0.2:5263,10.0.0.3:5263
export DJBOD_CLUSTER=$(djbod get-cluster-id)
djbod identity
djbod cluster show
djbod status
```

The CLI tries the listed entry points in order. This helps reach a
coordinator; it does not remove the coordinator's need to contact other
cluster members. Record the expected cluster UUID and pin it in clients
after discovery.

Run each node under the machine's service manager with the same service
account, configuration, and mounted disks on every restart. Order startup
after the required mounts and networking. During an upgrade, stop and
replace one node at a time, start it, then check `cluster show` and
`status`. Requests that need the stopped node can fail during that window.
Upgrade all nodes before introducing a configuration field older builds
cannot understand; unknown document fields are refused.

## Configuration reference

The node takes settings from command-line arguments, then environment
variables, then its TOML file, in that precedence order. A list supplied
outside the file replaces the file's list. `--config` or `DJBOD_CONFIG`
selects the file; there is no automatically discovered file. Without one,
supply a node ID, state directory, and at least one device another way.

| TOML field | CLI argument | Environment variable | Default or requirement |
| --- | --- | --- | --- |
| `node_id` | `--node-id` | `DJBOD_NODE_ID` | Required, persistent UUID |
| `listen` | `--listen` | `DJBOD_LISTEN` | `0.0.0.0:5263` |
| `advertise` | `--advertise` | `DJBOD_ADVERTISE` | Uses `listen`; set explicitly for wildcard listeners |
| `state_dir` | `--state-dir` | `DJBOD_STATE_DIR` | Required; stores `cluster.json` |
| `devices` | `--device` (repeatable) | `DJBOD_DEVICES` | Required; environment list is comma-separated |
| `bootstrap_peers` | `--bootstrap-peer` (repeatable) | `DJBOD_BOOTSTRAP_PEERS` | Empty; environment list is comma-separated |
| `temporary_max_age_secs` | `--temporary-max-age-secs` | `DJBOD_TEMPORARY_MAX_AGE_SECS` | `3600` |
| `stream_idle_timeout_secs` | `--stream-idle-timeout-secs` | `DJBOD_STREAM_IDLE_TIMEOUT_SECS` | `120` seconds without the next upload/shard frame |
| `allow_shared_filesystem` | `--allow-shared-filesystem` | `DJBOD_ALLOW_SHARED_FILESYSTEM` | `false`; experiments only |
| `tls.cert`, `tls.key`, `tls.ca` | `--tls-cert`, `--tls-key`, `--tls-ca` | `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, `DJBOD_TLS_CA` | Supply all three for a node using TLS |

Put node arguments after the subcommand, as in `djbod-node run --config
...`. `--log-format json` selects structured logs; `RUST_LOG=debug` enables
more detail. Logs go to standard error.

Cluster-wide settings live in the replicated cluster document, changed
through administration commands. Editing a local TOML file does not
change the cluster's coding scheme or transport mode.

| Initial setting | `init-cluster` argument | Default |
| --- | --- | --- |
| Data and parity counts | `--k`, `--m` | `3`, `1` |
| Block size | `--block-size` | `1048576` bytes (1 MiB) |
| Reserved free-space fraction | `--headroom` | `0.05` |
| Maximum key size | `--max-key-bytes` | `16384` bytes |
| Maximum object size | `--max-object-bytes` | `1099511627776` bytes (1 TiB) |
| Maximum user metadata | `--max-user-metadata-bytes` | `10485760` bytes (10 MiB), keys and values combined |
| Display name | `--name` or `DJBOD_CLUSTER_NAME` | Unnamed |

Use `djbod cluster-config` for the effective document. The CLI exposes
changes to scheme, limits, labels, name, addresses, and transport; there
is no `set-headroom` command.

## Store and retrieve objects

The CLI needs `--node`/`DJBOD_NODE` and `--cluster`/`DJBOD_CLUSTER` for
ordinary commands. `identity` and `get-cluster-id` need only an address.
All addresses are IP addresses with ports, including bracketed IPv6
addresses where appropriate. A cluster name is display text, not a
substitute for its UUID.

```sh
djbod put backups/archive.tar ./archive.tar --content-type application/x-tar
djbod head backups/archive.tar
djbod get backups/archive.tar ./restored.tar
djbod get notes/hello.txt                 # body on standard output
printf 'a small note\n' | djbod put notes/latest.txt -
djbod delete notes/latest.txt
```

File uploads stream. Standard-input uploads are buffered in memory first
because the server requires their length; use a named file for large
inputs. `head` reads metadata, not the object's body or parity. A
successful `head` therefore does not prove the stored data is intact.

Treat a download as valid only after the command succeeds. A checksum
error can arrive at the end of a stream. For a named output, the CLI
removes its partial file on failure; it cannot retract bytes already
written to standard output. Use a new destination filename: opening an
existing output can truncate it. Applications should use a temporary
destination and publish it only after a successful complete download.

An upload failure can leave an uncertain outcome, for example if the
connection fails after storage completes. Inspect the key before deciding
to retry, especially when another writer might use the same key. There
is no conditional-write or transaction API for coordinating writers.

### Listing and scripts

```sh
djbod list --prefix backups/ --limit 100
djbod --json list --prefix backups/ --limit 100
djbod --json head backups/archive.tar
```

Listings are ordered by key and paginated. CLI JSON listing output includes
`keys` and `truncated`. While `truncated` is true, request the next page
with `--start-after` set to the last returned entry's `key`, keeping the
same prefix. Library pages expose a `next_start_after` helper. Pagination
is not a snapshot under concurrent writes or deletes. The libraries also
offer `list_all`.

`--json` selects structured command results; scrub and drain emit one JSON
event per line. A GET body remains object bytes. Check process exit status
as well as output: ordinary operational errors exit 1, and maintenance
commands use 2 for findings, incomplete work, or reported failures, with
the JSON scrub caveat below. Argument parsing errors can also exit 2.
Retain standard error for context.

## Use the web interface

The UI accepts **one** node address, even when the CLI's `DJBOD_NODE`
contains several. Override it explicitly:

```sh
djbod-ui --node 10.0.0.1:5263 --cluster "$DJBOD_CLUSTER" --listen 127.0.0.1:5264
```

Open <http://127.0.0.1:5264/> on that machine, or forward the port:

```sh
ssh -L 5264:127.0.0.1:5264 nas1
```

The Overview, Objects, Maintenance, and Settings views expose status,
labels, upload/download, metadata, verification, repair, moving shards,
scrub, draining, and configuration changes. Verify reads the whole object
and checks it without saving a copy. Forced node removal and re-encoding
remain CLI procedures.

The browser connection is HTTP and has no login. Native TLS settings
protect the UI server's connection to a storage node, not the browser
connection. Keep the default loopback binding and use an SSH tunnel, or
provide authentication and HTTPS through a suitable reverse proxy. If
serving through a DNS name, allow that name with `--host`. Host and
same-origin checks are not user authentication.

The UI server remembers some failed downloads in memory and shows their
errors on the object panel. These notes do not cover other clients and
disappear on restart. Use verification and scrubs to establish health;
the UI is not a persistent damage ledger.

## Enable TLS

The initial `plain` mode has no network authentication or encryption.
Use TLS before making the native service available outside a trusted
network. There are no per-user permissions: an authenticated client can
read, write, delete, and administer the cluster.

### Issue and install certificates

From the repository, create a CA and one certificate per node and client:

```sh
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" init-ca --name home-nas
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" node nas1 10.0.0.1
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" node nas2 10.0.0.2
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" node nas3 10.0.0.3
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" client admin
scripts/djbod-pki.sh --dir "$HOME/djbod-pki" list
```

Node certificates need IP subject alternative names for their advertised
addresses, and both server and client authentication usage. Keep the CA
private key offline after issuance. Install only the CA certificate and
the relevant node certificate/private key on each node. The helper prints
paths and configuration; the equivalent OpenSSL commands are in the
[TLS walkthrough](getting-started.md#tls).

Append this table to each node's configuration, substituting its paths:

```toml
[tls]
cert = "/etc/djbod/nas1.crt"
key = "/etc/djbod/nas1.key"
ca = "/etc/djbod/ca.crt"
```

Make private keys readable only by the owning service/client account
(`chmod 600`); the software checks their permissions. Restart every node
to load its material while the cluster still uses `plain`.

### Migrate the running cluster

On the administration client:

```sh
export DJBOD_TLS_CA="$HOME/djbod-pki/ca.crt"
export DJBOD_TLS_CERT="$HOME/djbod-pki/admin.crt"
export DJBOD_TLS_KEY="$HOME/djbod-pki/admin.key"
djbod status
djbod cluster set-transport tls-optional
djbod status
djbod cluster set-transport tls
djbod status
```

| Mode | Between nodes | Clients |
| --- | --- | --- |
| `plain` | Plain TCP | Plain connections; TLS also accepted when material is loaded |
| `tls-optional` | Mutual TLS | Plain or TLS; a client certificate is optional |
| `tls` | Mutual TLS | TLS and a trusted client certificate required |

Leaving `plain` is refused if a node lacks TLS material. `tls-optional`
is a migration stage; it still admits unauthenticated clients. Giving a
client only `--tls-ca` encrypts the connection but supplies no client
identity and is insufficient under `tls`. The UI takes the same TLS
environment variables. A new node joining a TLS cluster needs its own
certificate and key configured before `join`.

Certificates are loaded from files at startup. Rotate them with file
replacement and restarts. To withdraw trust in an old certificate, rotate
the CA: first distribute a bundle trusting old and new CAs, reissue the
certificates that should remain, then remove the old CA from every trust
bundle and restart. There is no individual certificate revocation list.
Removing a node from membership does not itself revoke its certificate.

## Inspect and maintain the cluster

```sh
djbod status                       # device state and filesystem space
djbod cluster show                 # document versions and node builds
djbod cluster-config               # complete cluster document
djbod contents                     # record-derived counts per device
```

`contents` reports versions, distinct keys, blocks, and shard bytes without
reading payloads. It is useful for drain progress; it is not a corruption
scan or a filesystem allocation measurement. `status` space can include
filesystem overhead and unrelated files.

Assign memorable labels using UUIDs from the status/document output:

```sh
djbod cluster set-node-label <node-uuid> nas1
djbod cluster set-label <device-uuid> nas1-bay0
djbod cluster set-name 'Home storage'
djbod contents --node-id nas1
djbod contents nas1-bay0
```

Angle-bracketed values in this guide are placeholders, not literal shell
arguments. Labels are 1–128 bytes, contain no whitespace, and cannot look
like UUIDs. Device labels are unique among devices; node labels are
unique among nodes. Both can replace UUIDs in applicable CLI commands.
Clear a label or name using the respective command's `--clear` option.

### Scrub and repair

```sh
djbod scrub --rate-mib 50
djbod repair backups/archive.tar
djbod scrub --rate-mib 50 --repair
```

A cluster scrub checks local records and all shard blocks, including
parity, then checks metadata agreement and placement across nodes. The
rate is a cap in MiB/s **per node**, not a cluster-wide budget or a
measured speed. `--repair` attempts repairs for affected objects. A
successful repair run can exit 0 despite having found damage; incomplete
scrubs and failed repairs exit 2. Inspect the events and run a subsequent
scrub to confirm the result.

At this revision, `djbod scrub --json` can emit finding events and still
exit 0 when the scan completes. For alerts based on exit status, use the
ordinary text command, which exits 2 on findings without `--repair`.
Consumers of JSON must also inspect the finding and failure events; a
zero exit alone does not mean the scan was clean.

Schedule the online command from cron or a service timer on an
administration machine, with explicit binary paths, cluster settings, and
TLS credentials. Capture its output and nonzero exits. There is no built-in
schedule or persistent scrub history, so retain those reports yourself.
For a stopped node, a local check is:

```sh
djbod-node scrub --config /etc/djbod/node.toml --rate-mib 50 --json
```

The offline check cannot perform the online cross-node checks. Temporary
files past the configured age may be cleaned during local maintenance;
use copies if preserving all crash evidence matters.

### Document disagreement and address changes

Ordinary document changes require all existing nodes to be reachable and
in agreement. An interrupted update can leave different versions:

```sh
djbod cluster show
djbod cluster sync
djbod cluster show
```

Restore connectivity first. Sync moves lagging copies forward to the
highest valid version; it refuses equal-version documents with differing
contents. Do not hand-edit one running node's `cluster.json`.

To prepare an address move while the node still answers at its old
address, use `djbod cluster set-address nas1 10.0.0.7:5263`, then update
its local configuration to match before restarting. If it has already
moved, configure the new `listen`/`advertise` and start it: startup proposes
the address change to its peers. Keep bootstrap peer and client addresses
current, and reissue TLS certificates for new IPs. Although the document
can list several addresses, only the first address of each node is used.

## Grow and shrink storage

### Add a disk or a node

For a disk, prepare a new empty directory on a mounted filesystem, add its
path to `devices` in the owning node's configuration, then run while the
cluster is available:

```sh
djbod-node add-device --config /etc/djbod/node.toml --path /mnt/disk3/djbod
```

Restart that node to open the new device, then verify `status` and assign
a label. Another machine joins using the earlier `join` procedure. New
writes consider added devices immediately after they are available;
existing objects stay where they are. Automatic rebalance is not built.

Each write needs `k+m` active devices with enough room for an entire shard
file while preserving headroom. Selection favours the most free absolute
bytes. Adding a single large disk helps only when enough other eligible
devices can hold the remaining shards; total free bytes alone are not a
capacity guarantee.

### Move a shard or drain a device

```sh
djbod head backups/archive.tar
djbod move-shard backups/archive.tar 2 --to nas2-bay0
```

Shard indices start at zero. The destination must be active, have room,
and hold no other shard of that version. Omit `--to` to choose a target by
free space. A move copies an intact source or reconstructs it when
possible, updates placement records, then removes the old copy.

To retire a device:

```sh
djbod cluster set-state nas1-bay0 draining
djbod cluster drain nas1-bay0
djbod contents nas1-bay0
djbod cluster remove-device nas1-bay0
```

Marking it draining stops new placement and moves nothing. It continues
serving reads. Drain makes one pass and reports skipped versions; rerun
after correcting their causes. The initial estimate can refuse if targets
lack space or there are too few active devices. `--partial` deliberately
allows an incomplete pass and does not make the device safe to disconnect.

Proceed to removal only after a complete drain and zero remaining
versions; removal also checks cluster references. Remove the path from
the owning node's configuration and restart it before disconnecting the
disk. The device remains marked removed in the document. To cancel
retirement, set it `active`; completed moves stay completed.

A cluster with exactly `k+m` devices cannot empty one while retaining the
same scheme. Add suitable capacity or migrate objects to a narrower
scheme first. Mixed old and new schemes can also constrain a drain.

For a whole node, mark each of its devices draining, run `djbod cluster
drain --node-id nas1`, inspect the result, then `djbod cluster remove-node
nas1`. Ordinary node removal requires empty devices and the node's
acknowledgement; the removed process stops serving. Disable any service
manager restart for a node you have retired.

## Change the coding scheme and limits

```sh
djbod cluster set-scheme --k 4 --m 2 --block-size 1048576
djbod cluster reencode
djbod cluster set-limits --max-object-bytes 10737418240
djbod cluster set-limits --max-user-metadata-bytes 1048576
```

`set-scheme` changes future writes and requires enough active devices.
Existing objects retain their recorded scheme and can coexist indefinitely.
`reencode` is optional: it streams each object using a different scheme
through the client as a GET followed by a replacement PUT, preserving
content type and user metadata. It creates a new version and creation time.
Allow room for old and new data during each replacement, and avoid
concurrent application writes to the keys being migrated. Interrupted work
can be rerun; inspect per-key failures and the final exit status.

Supported bounds are `1 <= k <= 32`, `0 <= m <= 8`, and `k+m <= 64`.
Block size must be a multiple of 4096 between 64 KiB and 64 MiB. `m=0`
provides no protection against a lost shard. Limits apply to new requests;
lowering a limit does not rewrite stored objects.

Ignoring metadata, file headers, padding, filesystem overhead, and reserved
space, an object's encoded payload is about `(k+m)/k` times its original
size. Thus `3+1` is about 1.33 times and `4+2` is 1.5 times. These are
coding ratios, not measured capacity or performance results.

## Handle failures and recover data

### A failed request

Keep the error's node/device UUID, key, version, shard index, and stripe
when present. Check connectivity and `cluster show` before assuming data
is lost. Map UUIDs to physical devices using labels and your configuration.
Do not edit metadata JSON to hide a checksum error.

For corruption or a missing shard on a reachable device, run `djbod repair
<key>`, inspect the repair report, then fetch/verify and scrub. Repair
needs sufficient trustworthy records and intact blocks; it cannot invent
data when those are gone. A listed but unreachable node normally must
return before repair can proceed.

For a failed device whose node still answers, a shard can sometimes be
moved to a spare using `move-shard`, reconstructed from survivors. Record
the affected keys and placements and inspect every result. Do not mark a
referenced device removed just to bypass an error.

### A permanently lost node

Only after establishing that a node is permanently unavailable:

```sh
djbod cluster remove-node <node-uuid> --force
```

The command refuses if the node answers. Otherwise it reports affected
objects and those whose recorded placements put more than `m` shards on
that node, asks you to type its UUID, removes membership, and attempts
repair onto remaining devices. Other corruption and absent metadata can
make actual loss worse than the placement estimate. A host with several
shards of an object can exceed the redundancy budget in one failure.

Read the report before confirming. `--yes` bypasses the interactive
confirmation for deliberate automation. Forced removal can complete its
membership change while some repairs fail because of missing data or
capacity; inspect the result and follow with `scrub --repair` after
addressing the cause. A returning removed node adopts the newer document
and refuses to serve. `--wipe-removed-device` on setup commands destroys
old contents before reuse; it is not a recovery step.

### Recover without running nodes

Stop processes using the source disks, or work from stable copies. The
recovery tool reads device trees directly and never modifies those trees:

```sh
djbod-recover list /recovery/disk0/djbod /recovery/disk1/djbod /recovery/disk2/djbod
djbod-recover extract backups/archive.tar --out /recovery-output/archive.tar \
  /recovery/disk0/djbod /recovery/disk1/djbod /recovery/disk2/djbod
```

Create the destination directory first. `extract` refuses an existing
output file. It selects the newest version found unless you supply
`--version <version-id>` from `list`. At least one valid, usable metadata
record and enough intact shards for each stripe must survive. For a `3+1`
object, three intact shard files suffice. Different objects may need
different source disks, so provide all surviving device trees.

Neither a running cluster nor `cluster.json` nor intact device identity
files are required. The tool verifies blocks and the reconstructed
object, and reports conflicting or damaged input. Finding a version in
`list` alone does not guarantee successful extraction. Recovery cannot
restore data already deleted or overwritten and cleaned up.

### Backups

Keep object backups outside the cluster, and test restoration. Save node
configurations, mount mappings, UUID/label inventories, a cluster-document
copy, and the software revision for operational recovery. Protect TLS
material separately. The small state directory is configuration, not an
object backup. An arbitrary live copy of shard trees is not a coordinated
snapshot; quiesce writes and maintenance or stop nodes before making a
consistent filesystem copy intended for recovery.

## Use the client libraries

The Rust [`djbod-client`](../crates/djbod-client/src/lib.rs) crate provides
async and blocking clients, streaming transfers, paginated listing, TLS,
error details, and administration helpers. Use `put_from_reader` and
`get_to_writer` for large bodies. Streaming transfers are not transparently
replayed after failure; the caller must decide whether and how to retry.
Always consume the final stream result before accepting the body.

The Python package wraps the blocking Rust client. Build it using its
[package instructions](../crates/djbod-python/README.md), then:

```python
import djbod

client = djbod.Client(["10.0.0.1:5263", "10.0.0.2:5263"])
client.put_file("backups/archive.tar", "archive.tar",
                content_type="application/x-tar")
info = client.head("backups/archive.tar")
print(info.size, info.version)
client.get_to_file("backups/archive.tar", "restored.tar")
for entry in client.list_all(prefix="backups/"):
    print(entry.key)
```

Pass `cluster="<uuid>"` to pin the expected cluster and `tls_ca`,
`tls_cert`, and `tls_key` for TLS. The Python constructor does not read
the CLI's environment settings for you. Methods block while releasing
the Python interpreter lock. `put`/`get` hold the body in memory; the file
methods stream. Consult the [type declarations](../crates/djbod-python/python/djbod/_native.pyi)
for the Python surface; it does not wrap every Rust administration method.

## Troubleshooting

| Symptom | Check and next step |
| --- | --- |
| `InsufficientDevices` | Count active devices with room for one full shard each, allowing headroom; check the object's scheme. Add capacity or choose an appropriate scheme. |
| A node cannot be reached | Check the process, IP/port, firewall, advertised address, and TLS settings. Another entry point does not make that member optional. |
| `DocumentVersionMismatch` | Restore reachability, inspect `cluster show`, then use `cluster sync`. |
| `RecordsInconsistent` | Preserve evidence and run a scrub. Repair can restore some missing or interrupted record updates; conflicting trusted records require investigation. |
| A read reports corruption or a missing shard | Keep the detailed error, repair the key, then verify the complete object and parity with a scrub. |
| `TlsRequired` or a certificate error | Check mode, CA bundle, client certificate/key, IP SANs, expiry, and private-key permissions. |
| Startup rejects shared filesystems | Check mounts and physical disk mapping. Enable the bypass only for a disposable experiment. |
| Setup rejects a nonempty device | Verify the path and identity. Use an empty device; do not erase existing storage to make setup pass. |
| Drain skips versions | Read the per-version causes, add eligible targets or repair damage, then rerun. Do not disconnect the source yet. |
| UI rejects a comma-separated address | Supply a single `--node`; multiple entry points are a CLI/library feature. |
| Upload times out after a pause | Check the sender and network; adjust `stream_idle_timeout_secs` if legitimate pauses exceed the configured per-frame timeout. |

For the exact options in an installed build, use `djbod --help`, `djbod
cluster --help`, and each command's `--help`.
