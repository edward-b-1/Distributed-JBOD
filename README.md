# Distributed-JBOD

A distributed, resilient, object store.

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
shard, so any one of the four devices may be lost. In another terminal,
with the cluster id `init-cluster` printed:

```sh
export DJBOD_NODE=127.0.0.1:5263
export DJBOD_CLUSTER=<the cluster id>

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
  metadata record carries a checksum. A read that meets a bad block says
  so, naming the disk, the shard, and the stripe, and `djbod repair`
  rebuilds the shard from the others, onto another disk if its own is
  gone. A scrub checks every disk on a schedule you set, then checks
  across the cluster that every object's records agree and every shard is
  where its record says, and can repair what it finds.
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
