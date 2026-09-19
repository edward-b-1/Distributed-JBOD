# Distributed-JBOD

A distributed, resilient, object store.

Distributed-JBOD solves the problem of a user who requires a large pool of network connected storage, but who does not have access to datacenter grade server hardware. Connect a few commodity devices together, and run them with whatever storage devices are available. Storage devices and nodes are easy to add and remove, providing a way to extend or shrink the storage pool over time.

Distributed-JBOD can aggregate a mismatched pool of disks (JBOD) from across multiple devices. It is intended to be used with low-cost commodity machines. A Distributed-JBOD system can combine multiple hosts together to create a durable, bitrot-protected pool of storage space. The parameters are configurable so that the balance between storage efficiency and durability can be adjusted.

Every node runs the same process; there is no master, and no metadata lives on any single device. Objects are split into stripes and erasure-coded with a systematic Reed-Solomon code, every
block carries a checksum, and every operation is fail-stop: damage is
reported eagerly, and the tools to find and repair damage are provided.

The design is in [SPEC.md](SPEC.md), written before the code and kept in
step with it; every decision is numbered and marked as decided, proposed,
open, or deferred. Appendix C of the specification holds the crate layout,
the milestones, and the status of each.

## What it does

- **Erasure coding with a global scheme.** `k` data and `m` parity shards
  per stripe, chosen for the cluster and changeable later; every object
  records the scheme it was written with, so old and new coexist.
- **Bitrot protection.** XXH3-64 on every block and on every whole object;
  a checksum on every metadata record. A read that meets a bad block
  fails and names the device, shard, and stripe.
- **Placement by free space.** New shards go to the emptiest active
  devices, one shard per device; where each shard is lives in a JSON
  record replicated to every holder, not in a hash.
- **Scrub and repair.** A per-node scrub of every record and block, a
  cluster-wide scrub with cross-node checks, and repair that rebuilds a
  damaged or lost shard from the others.
- **Administration without downtime.** Move a shard, drain a disk, remove
  a device or a node (or force out a dead one), relabel devices, change
  the scheme and re-encode, change the size limits, all as versioned
  changes to one cluster document that every node holds.
- **Recovery with nothing running.** `djbod-recover` lists and extracts
  objects from bare device directories, needing only `k` intact shards.
- **Optional TLS.** Mutual TLS between nodes and client certificates, from
  one certificate authority you manage with `openssl`; three transport
  modes so a cluster can move from plain to TLS without a stop.
- **Two ways in.** The `djbod` command-line client, and `djbod-ui`, a web
  page and JSON API that speak the same native protocol.

## Binaries and tools

| Binary | Purpose |
|--------|---------|
| `djbod-node` | The node process: `init-cluster`, `join`, `add-device`, `run`, and an offline `scrub`. Every setting can come from the configuration file, an argument, or a `DJBOD_*` environment variable. |
| `djbod` | The client and administration tool: `status`, `put`, `get`, `head`, `delete`, `list`, `repair`, `move-shard`, `scrub`, and `cluster` with `show`, `sync`, `set-state`, `set-label`, `drain`, `remove-device`, `remove-node`, `set-scheme`, `reencode`, `set-transport`, `set-limits`. `--json` everywhere. |
| `djbod-ui` | The administration web UI: one embedded page and an API under `/api`, each call one native operation. Binds to localhost by default. |
| `djbod-recover` | The offline recovery tool: `list` and `extract` from device directories. |
| `scripts/djbod-pki.sh` | Issues the certificate authority and the node and client certificates with `openssl`. |

Crates: `djbod-core` (on-disk format, checksums, coding), `djbod-proto`
(the native protocol), `djbod-node`, `djbod-cli`, `djbod-recover`, and
`djbod-ui`.

## Trying it

[docs/getting-started.md](docs/getting-started.md) walks through building
the binaries, creating a cluster of one node with directories as devices,
storing and fetching objects with the `djbod` client, administering the
cluster from a browser with `djbod-ui`, breaking things on purpose to see
the fail-stop behaviour and repair them, naming, draining, and removing
devices, changing the scheme, recovering objects with no cluster running,
adding a second node, and turning on TLS.

```sh
cargo build --release
cargo test --workspace     # starts nodes on localhost ports; takes a few seconds
```

## Status

Milestones 1 to 6 of the plan in Appendix C are built: the core format, a
single node and client, the cluster with membership changes and the
cluster-wide scrub, administration (re-placement, drain, removal,
recovery, re-encode, size limits), TLS, and the web UI. Milestone 7,
damage marks that remember what the checks found (SPEC 20.7), is designed
and next to build. Design proposals under discussion live in
[docs/proposals](docs/proposals).

The system has been tested by many nodes running in one process on one
machine and by hand against the built binaries. It has not yet run for
long on several real machines; that is the next thing to do before
relying on it.

## Security posture

By default nothing on the wire is authenticated or encrypted, which is
fine on a LAN whose members you trust and nowhere else. With TLS enabled
(SPEC 19.1.6) nodes authenticate each other and clients present
certificates; there is not yet any authorisation beyond that, so every
authenticated client may do everything, including administration. Key
material is only ever read from files named by path, never from the
configuration file or the cluster document.

## Default port

Nodes listen on TCP port **5263** by default. It spells JBOD on a telephone
keypad (J=5, B=2, O=6, D=3), and IANA lists it as unassigned. The first
candidate, 7400, turned out to be the DDS/RTPS discovery port used by ROS 2,
so it was dropped. See SPEC.md item 6.1.1.
