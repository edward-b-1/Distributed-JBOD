<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/brand/djbod-lockup-horizontal-onDark.svg">
  <img src="docs/brand/djbod-lockup-horizontal.svg" alt="Distributed JBOD" width="300">
</picture>

# Distributed-JBOD

A distributed, resilient, object store. Built with Rust.

Distributed-JBOD solves the problem of a user who requires a large pool of
network connected storage, but who does not have access to datacenter grade
server hardware. Connect a few commodity devices together, and run them
with whatever storage devices are available. Storage devices and nodes are
easy to add and remove, providing a way to extend or shrink the storage
pool over time.

Distributed-JBOD can aggregate a mismatched pool of disks (JBOD) from
across multiple devices. It is intended to be used with low-cost commodity
machines. A Distributed-JBOD system can combine multiple hosts together to
create a durable, bitrot-protected pool of storage space. The parameters
are configurable so that the balance between storage efficiency and
durability can be adjusted.

Every node runs the same process; there is no master, and no metadata
lives on any single device. Objects are split into stripes and
erasure-coded with a systematic Reed-Solomon code, every block carries a
checksum, and every operation is fail-stop: damage is reported eagerly,
and the tools to find and repair damage are provided.

## Quick start

Build, then create a cluster of one node with four directories standing in
for four disks:

```sh
cargo build --release

mkdir -p /tmp/djbod/state /tmp/djbod/d0 /tmp/djbod/d1 /tmp/djbod/d2 /tmp/djbod/d3
cat > /tmp/djbod/node.toml <<'TOML'
node_id = "6a2d5c1e-3f0b-4b1a-9d2e-0c7e8a9b1f22"
listen = "127.0.0.1:5263"
state_dir = "/tmp/djbod/state"
devices = ["/tmp/djbod/d0", "/tmp/djbod/d1", "/tmp/djbod/d2", "/tmp/djbod/d3"]
allow_shared_filesystem = true      # only because these four directories share one disk
TOML

target/release/djbod-node init-cluster --config /tmp/djbod/node.toml --k 3 --m 1
target/release/djbod-node run --config /tmp/djbod/node.toml
```

`--k 3 --m 1` splits every object into three data shards and one parity
shard, so any one of the four devices may be lost. In another terminal:

```sh
export DJBOD_NODE=127.0.0.1:5263
export DJBOD_CLUSTER=$(target/release/djbod get-cluster-id)   # or paste what init-cluster printed

target/release/djbod put photos/cat.jpg cat.jpg --content-type image/jpeg
target/release/djbod list --prefix photos/
target/release/djbod get photos/cat.jpg copy.jpg
target/release/djbod status
```

For the web interface, `target/release/djbod-ui` with the same two
variables set, then open http://127.0.0.1:5264/.

[docs/getting-started.md](docs/getting-started.md) continues from here:
breaking things on purpose and repairing them, naming, draining, and
removing disks, adding a second machine, changing the scheme, recovering
objects with no cluster running, and turning on TLS.

## What you get

- **Any disks, any machines.** Devices are plain directories on ordinary
  filesystems, of any size, on any number of machines. Each object is
  placed on the emptiest disks at the time it is written, one shard per
  disk, so a pool of mismatched sizes fills evenly and a new disk starts
  taking writes at once.
- **Erasure coding you choose.** `k` data and `m` parity shards per
  object. `3+1` tolerates one lost disk for 33% overhead; `4+2` tolerates
  two for 50%; `1+1` is plain mirroring. Change it later: every object
  records the scheme it was written with, so old and new objects coexist
  and you re-encode at your own pace, or never.
- **Bitrot found and fixed.** Every block, every object, and every
  metadata record carries an XXH3-64 checksum, fast enough to verify at
  disk speed; keys are located by their SHA-256. A read that meets a bad
  block says so, naming the disk, the shard, and the stripe, and `djbod
  repair` rebuilds the shard from the others, onto another disk if its
  own is gone. A scrub checks every disk on a schedule you set, then
  checks across the cluster that every object's records agree and every
  shard is where its record says, and can repair what it finds.
- **Grow, shrink, and fix while running.** Add a disk or a machine, name
  it, mark it draining so it takes no new data, move its shards off,
  remove it; force out a machine that will never come back and rebuild
  what it held; change the scheme or the size limits. Each is one
  command, and each is a versioned change to one cluster document that
  every node holds a copy of, so the cluster never depends on any one
  node or disk for its own configuration.
- **Nothing hidden.** Where each object's shards are is written down in a
  small JSON record replicated to every disk that holds a shard, not
  computed from a hash, and objects live as one record and one shard file
  per disk in a directory layout you can read. `djbod-recover` gets your
  objects back from bare disks with no node running, needing only `k`
  intact shards.
- **No special node.** Every node is the same program. Any node answers
  any request. Configuration is one small file, and every setting can also
  be an argument or an environment variable.
- **Optional TLS.** Mutual TLS between nodes and client certificates from
  a certificate authority you create with `openssl`, or the included
  script. A running cluster switches from plain to TLS in two steps.

## Alternatives?

Distributed-JBOD is designed for a specific use case: non-uniform nodes
with non-uniform storage devices. The typical deployment is a few small
machines with whatever disks are available, with erasure coding enabled
across all of them, every block checked on every read, and nothing else
to run.

- **MinIO** erasure-codes S3 storage, but lays it out in erasure sets of
  uniform drives and grows by adding whole pools. A pile of odd-sized
  disks either wastes the difference or cannot be laid out at all.
  Distributed-JBOD places each object on the emptiest disks at the time
  it is written, so any mix of sizes fills evenly and one new disk starts
  taking writes at once. MinIO's community edition also changed terms
  in 2025; check them before depending on it.
- **Garage** is built for the same hardware, heterogeneous and
  unreliable machines with no master, and is the closest relative. It
  keeps three full copies of every object rather than erasure coding,
  so it spends 3x raw space where `3+1` spends 1.33x, and it does not
  verify each block against a checksum as it is read.
- **SeaweedFS** is a fast volume-based blob store whose erasure coding is
  a background tier for cold volumes, applied after the fact. It needs
  master servers, and a filer for anything beyond flat blobs. Here
  erasure coding is the write path, and there is no master to keep up.
- **Ceph** does everything here and a great deal more, and is the right
  answer at scale. It also needs monitors, managers, several
  well-provisioned nodes to be sensible, and the operations knowledge to
  run them, which is why small deployments avoid it.

What none of them offer is the recovery story: each object here is a
plain JSON record beside a shard file on each disk, in a layout you can
read, and `djbod-recover` reads the objects back from bare disks with
nothing running. Where they win, they win clearly: S3 compatibility,
scale, and years of production use. Distributed-JBOD is young, as the
[Status](#status) section says.

## The tools

| Tool | What it is for |
|------|----------------|
| `djbod-node` | The node process. `init-cluster`, `join`, `add-device`, `run`, and an offline `scrub` of one machine's disks. |
| `djbod` | The client and administration tool: `put`, `get`, `head`, `list`, `delete`, `status`, `repair`, `scrub`, `move-shard`, and `cluster` for membership and settings. `--json` everywhere. |
| `djbod-ui` | A web page for the same operations: status, devices, objects, upload and download, scrub, drain, repair. Binds to localhost. |
| `djbod-recover` | `list` and `extract` objects from device directories with nothing running. |
| `scripts/djbod-pki.sh` | Issues the certificate authority and node and client certificates. |

## Security

Out of the box nothing on the wire is authenticated or encrypted. That is
fine on a home or office network whose members you trust, and unsafe
anywhere else. Enable TLS (see the guide) to authenticate nodes and
clients and encrypt every connection. There is no per-user authorisation
yet: every authenticated client may do everything, including
administration. The web UI has no login of its own and listens on
localhost by default; put it behind something that does if you expose it.

Nodes listen on TCP port **5263** by default, which spells JBOD on a
telephone keypad.

## A typical multi-node deployment

The quick start runs one node with directories standing in for disks.
The intended shape is one node per machine, each with its own disks.
Three machines, `nas1` to `nas3` at `10.0.0.1` to `10.0.0.3`, with three
disks each, look like this. Give every machine a fixed address, by a
DHCP reservation if nothing else, because the cluster document records
where each node is reached.

**1. Mount the disks** on each machine, one filesystem per disk, and
make a directory on each for djbod. Any size, any filesystem; they need
not match each other or the other machines:

```sh
sudo mkdir -p /mnt/disk0/djbod /mnt/disk1/djbod /mnt/disk2/djbod /var/lib/djbod
```

**2. Write `/etc/djbod/node.toml`** on each machine. Only `node_id`,
`listen`, and `bootstrap_peers` differ between them; `uuidgen` makes the
id. On `nas1`:

```toml
node_id = "5f0c1d2e-8a9b-4c3d-9e1f-2a3b4c5d6e7f"     # this machine's, for life
listen = "10.0.0.1:5263"                              # this machine's own address
state_dir = "/var/lib/djbod"                          # its copy of the cluster document
devices = ["/mnt/disk0/djbod", "/mnt/disk1/djbod", "/mnt/disk2/djbod"]
bootstrap_peers = ["10.0.0.2:5263", "10.0.0.3:5263"]  # the others, asked at startup
```

If a machine listens on every interface, set `listen = "0.0.0.0:5263"`
and `advertise = "10.0.0.1:5263"` so the others know which address to
use.

**3. Create the cluster on the first machine**, then start its node:

```sh
djbod-node init-cluster --config /etc/djbod/node.toml --name home-nas --k 4 --m 2
djbod-node run --config /etc/djbod/node.toml
```

`init-cluster` prints the cluster id, and any running node repeats it to
`djbod get-cluster-id --node <address>`. `4+2` puts six shards of
every object on six different disks and survives any two of them
failing, for 50% overhead; `3+1` costs 33% and survives one. Nine disks
is comfortably more than the six a `4+2` write needs, and the choice can
be changed later. Until enough disks have joined, writes are refused and
say so; the first node alone cannot hold a `4+2` object.

**4. Join the other machines**, each with its own configuration file,
pointing at any node already running, then start them:

```sh
djbod-node join --config /etc/djbod/node.toml --peer 10.0.0.1:5263 --cluster <the cluster id>
djbod-node run --config /etc/djbod/node.toml
```

`join` initialises the new disks and proposes a new version of the
cluster document listing the machine and its disks, which every running
node must accept. Run each node under `systemd` or whatever keeps
services alive on that machine, so it restarts with it; at startup a
node asks its bootstrap peers for a newer document, so a machine that
was off while the cluster changed catches up on its own.

**5. Use it from any machine** on the network. Every node answers every
request, so point the client at whichever is nearest:

```sh
export DJBOD_NODE=10.0.0.1:5263
export DJBOD_CLUSTER=$(djbod get-cluster-id)   # any node tells you; --json adds the name
djbod cluster show          # three nodes, one document version, each node's build
djbod status                # nine disks, their labels, state, and free space
djbod put backups/2026-09.tar backup.tar
```

Give the disks and machines names once, so `status` reads as your
hardware does: `djbod cluster set-label <device-uuid> nas1-disk0` and
`djbod cluster set-node-label <node-uuid> nas1`. For the web UI, run
`djbod-ui` on one machine with the same two variables; it binds to
localhost, so reach it over an SSH tunnel or put it behind something
with a login.

**What to expect.** Shards are placed one per disk on the emptiest disks,
without regard to which machine a disk is in. A machine that is switched
off takes its three disks with it, so under `4+2` an object with three
shards on that machine is unreadable until the machine returns, and the
request says so. Nothing is lost unless disks themselves fail, and at
most two disks may fail before an object is gone. Adding a fourth
machine later is step 4 again; adding a disk to a machine is
`djbod-node add-device` and a restart; a machine whose address changes
is moved with `djbod cluster set-address`, or simply restarted with the
new address configured. Switch the cluster to TLS before it leaves a
network you trust; the guide has the steps.

## For developers

The design is in [SPEC.md](SPEC.md), written before the code and kept in
step with it: every decision is numbered and marked as decided, proposed,
open, or deferred, and the reasoning behind rejected alternatives is kept.
Appendix C has the crate layout and the milestone plan. Proposals under
discussion live in [docs/proposals](docs/proposals). The crates are
`djbod-core` (on-disk format, checksums, coding), `djbod-proto` (the
native protocol), `djbod-node`, `djbod-cli`, `djbod-recover`, and
`djbod-ui`; `cargo test --workspace` runs everything, starting nodes on
localhost ports, and takes a few seconds.

## Status

Milestones 1 to 6 of the plan are built: the core format, a single node
and client, the cluster with membership changes and the cluster-wide
scrub, administration (re-placement, drain, removal, recovery, re-encode,
size limits), TLS, and the web UI. Milestone 7, damage marks that remember
what the checks found, is designed and next to build.

The system has been tested by many nodes running in one process on one
machine and by hand against the built binaries. It has not yet run for
long on several real machines; do that before relying on it.

## License

Distributed-JBOD is free software under the GNU Affero General Public
License, version 3 only (`AGPL-3.0-only`); the full text is in
[LICENSE](LICENSE). Copyright (C) 2026 edward-b-1.

You may run, study, change and share it. If you distribute it, or offer a
modified version to others over a network, which a storage service and
its web UI do, you must offer them the source of the version they use.
The web UI links to this repository for that reason. The binaries also
contain third-party packages under permissive licenses, whose notices are
collected in [THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES); ship that file
beside any binary you distribute.
