# Concepts

## Nodes, devices, and the cluster

A **node** is one `djbod-node` process on one machine. It listens on one
address, usually port **5263**. Every node runs the same program. Any node
can answer a client. There is no separate master process.

A **device** is one directory on one filesystem, normally one disk mounted
on its own. The node is told the paths in its configuration file. On first
use it writes `DISTRIBUTED-JBOD-DEVICE.json` into an empty directory and
from then on that directory belongs to one cluster. The device's permanent
name is a UUID in that file. The path is only on that machine.

The **cluster document** is one JSON file, `cluster.json`, in each node's
state directory. Every node holds a copy. It lists the nodes, the
addresses clients and peers use, the devices and which node owns each, the
erasure scheme, the size limits, and the transport (`plain`,
`tls-optional`, or `tls`). Changes to it are numbered. `djbod cluster show`
prints the version each node holds. `djbod cluster sync` pushes the
highest version out to every node that answers.

A node's configuration file is not the cluster document. It is that
machine's own paths, its UUID, its listen address, and, when you use TLS,
the certificate files. Membership lives in the cluster document.
`djbod-node` will not invent a disk that the file does not list, and a
disk that is only in the file is not in the cluster until `init-cluster`,
`join`, or `add-device` puts it there.

## Objects, shards, and k + m

An object is stored under a key, such as `photos/cat.jpg`. A key is a
string, up to the cluster's key-length limit (16 KiB unless you change
it). The current bytes of a key are one **version**. Writing the same key
again stores a new version and the old one is replaced.

Each version is split into stripes and erasure-coded. The scheme is two
numbers:

- `k` is the number of data shards.
- `m` is the number of parity shards.

Any `k` of the `k + m` shards are enough to rebuild the object. The
cluster stores one shard on each of `k + m` different devices.

| Scheme | What one failure costs | Overhead |
|---|---|---|
| `1+1` | One lost shard. This is a mirror: two copies. | 100% |
| `3+1` | One lost shard. | 33% |
| `4+2` | Two lost shards. | 50% |
| `k+0` | A checksum on every block, and no spare shard. The first lost shard loses the object. | none |

`m` is a property of how the object was written. It is not a promise that
the cluster keeps serving while `m` machines are switched off. See
[When a machine is off](#when-a-machine-is-off).

The scheme is a field of the cluster document, so new writes all use the
same one. Each object's record remembers the scheme it was written with.
`djbod cluster set-scheme` changes the document and moves no bytes. Old
objects stay readable. `djbod cluster reencode` rewrites them one at a
time onto the new scheme.

The default stripe block is 1 MiB. `init-cluster --block-size` and
`cluster set-scheme --block-size` take a multiple of 4096 between 64 KiB
and 64 MiB.

## Where a shard is placed

For each write, the cluster picks the `k + m` active devices with the most
free space that it can currently read, and puts one shard on each. It does
not try to put those shards on different machines. Two shards of one
object can sit on two disks in the same server.

A device marked `draining` receives nothing new. A device the node cannot
read is skipped, and the write says so. If fewer than `k + m` usable
devices have room, the write stores nothing and fails with
`InsufficientDevices`.

Free space is the filesystem's free bytes minus a **headroom** fraction
of the filesystem size (5% unless `init-cluster --headroom` said otherwise)
minus space already reserved for writes in flight. Headroom is fixed at
cluster creation. There is no later command to change it.

## Names

You will see three kinds of name.

- The **cluster id** is a UUID. `djbod --cluster` and `DJBOD_CLUSTER` take
  it. `init-cluster` prints it. `djbod get-cluster-id --node <address>`
  prints it again, with no `--cluster`. A **cluster name**, set with
  `init-cluster --name` or `djbod cluster set-name`, is shown beside the
  id. The name can contain spaces. The id is what the client sends.
- A **node id** is the UUID in that machine's configuration. It does not
  change when the address changes. A **node label** (`djbod cluster
  set-node-label`) is a short name, unique in the cluster, 1 to 128
  bytes, with no spaces and not itself a UUID.
- A **device id** is the UUID in `DISTRIBUTED-JBOD-DEVICE.json`. A
  **device label** (`djbod cluster set-label`) has the same rules as a
  node label. Commands that take a device or a node accept either the
  UUID or the label.

## Device state and availability

The cluster document stores a **state** for each device. That is a
decision you record:

| State | Meaning |
|---|---|
| `active` | Eligible for new shards. |
| `draining` | Serves what it already holds. Receives no new shard. `djbod cluster drain` is allowed to move its shards off. |
| `removed` | Retired. The id stays in the document so the same disk is recognised if it reappears. It is not a place to store anything. |

**Unavailable** is not a state. It is what a node reports when it cannot
read a device it still owns: the directory is missing, the disk is not
mounted, or `DISTRIBUTED-JBOD-DEVICE.json` cannot be stat'd. `djbod
status` prints `active, unavailable` or `draining, unavailable`. The
document still says `active` or `draining` until you change it.

A removed device that the node does not have open disappears from `djbod
status` after the node is restarted without that path. The document still
lists it as `removed`.

## When a machine is off

Membership is not quorum. The operations that look at records ask every
node in the document. If one of them does not answer, the operation fails
with `NodeUnreachable` and names that node.

That includes:

- `djbod status`
- `djbod get` and `djbod put`, including objects whose shards are all on
  machines that are still up
- `djbod repair`, `djbod scrub`, and `djbod cluster drain`

`djbod cluster show` still answers. The down node is a row whose version
column says `unreachable:` and the reason. `djbod identity` and `djbod
get-cluster-id` talk to one address, so they work against any node that
is up.

When the machine comes back, the node reads its saved document, asks its
`bootstrap_peers` for anything newer, and serves again. The reads that
failed start working without a repair.

If the machine is not coming back, the cluster stays in that failed state
until you remove the node. The ordinary `djbod cluster remove-node`
cannot, because the node has to acknowledge it. `djbod cluster remove-node
--force` is the command for a node that does not answer. It rebuilds
shards that lived only on that node onto other devices, and it can only
do that when some other device has room and does not already hold a shard
of that object. [Scenarios](scenarios.md#a-node-will-never-come-back)
walks through both the case that rebuilds and the case that loses the
object.

## Damage the cluster can still read

These are different from a machine that is off or a disk that is gone,
because the nodes are up and the record copies can still be gathered.

- A shard block whose checksum does not match, or a shard file that has
  been deleted, is reconstructed for that read. `djbod get` writes the
  correct bytes and exits 2. The damage is still on disk. Every later
  read of that object pays the reconstruction cost until `djbod repair
  <key>`.
- A write that had to skip a device the cluster cannot read stores the
  object on other devices. `djbod put` prints those devices and exits 2.

Exit 2 in both cases means the bytes you asked for were handled and the
cluster is not whole. Exit 1 means the operation did not succeed: the
object was not stored, or the read did not produce the object. A `get`
that exits 1 removes a partial output file when the output was a named
file.

`djbod scrub` is how you find damage before a client does. It does not
run on its own. [Scenarios](scenarios.md#finding-damage-before-a-client-does)
has the exit codes and a cron line.

## What the cluster will not do

Nothing moves data unless you run a command that says it will: `repair`,
`scrub --repair`, `move-shard`, `cluster drain`, `cluster reencode`, or
`cluster remove-node --force`. There is no alert daemon. The signal is
the command's exit code and the line it prints. A scheduled `djbod scrub`
whose exit code is checked is the health check.
