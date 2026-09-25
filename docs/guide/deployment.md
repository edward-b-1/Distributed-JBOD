# Deployment

A production layout is one `djbod-node` per machine, one filesystem per
disk, and no `allow_shared_filesystem`. The cluster document records
each node's address, so give every machine a fixed address before you
create the cluster. A DHCP reservation is enough.

## Choose a scheme before the first write

`k + m` devices must be active before a write succeeds, and a later
drain or forced removal needs somewhere to put a rebuilt shard that
does not already hold one. Size the pool for the failures you intend to
repair, not only for the failures you intend to survive on paper.

| You have | A scheme that fits | What you can repair |
|---|---|---|
| One server, four or more disks | `3+1` (needs 4) or `2+1` (needs 3) | One dead disk, after `remove-device --force` and `scrub --repair`, as long as `k + m` other disks remain or you add a replacement first. The server itself being off takes the cluster with it. |
| Three servers, several disks each | `4+2` or `3+1`, with more disks than `k + m` | One or two dead disks, depending on `m`. A server that is off stops every `get`, `put`, and `status` until it returns or you force-remove it. Force-removal rebuilds only onto disks that do not already hold a shard of that object. |
| Several small machines and you expect one to be off | Keep every machine powered, or plan to force-remove the one that is down. `1+1` does not keep reads working through a missing machine. | A mirror still needs both record copies present to serve the object. A missing node fails the read even when the other copy is on a machine that is up. |

`4+2` on nine disks is a comfortable pool: a write needs six, and you
can lose two disks and still have a destination for a rebuild. `3+1`
on one server with a spare bay is the same idea at smaller scale. You
can change the scheme later with `djbod cluster set-scheme` and
`djbod cluster reencode`. Objects written under the old scheme stay
readable either way.

Because placement ignores which machine a disk is in, an object can
land several shards on one server. That is normal. It means a dead
server can hold more than `m` shards of some objects. Those objects
cannot be rebuilt if you force-remove the server. The graceful
replacement in [Scenarios](scenarios.md#replace-a-node) moves the
shards off while the server is still up, which is the path that does
not depend on `m`.

## Prepare the disks

One filesystem per disk. Create a directory on it for the device, and a
state directory on the machine's system disk. The state directory holds
only the cluster document. It should not be one of the device paths.

```sh
sudo mkfs.ext4 /dev/disk/by-id/ata-EXAMPLE
sudo mkdir -p /mnt/disk0
sudo mount /dev/disk/by-id/ata-EXAMPLE /mnt/disk0
sudo mkdir -p /mnt/disk0/djbod /var/lib/djbod /etc/djbod
```

ext4 reserves five percent of the filesystem for root unless you change
it. That reservation is on top of the cluster's own headroom. For a disk
that will only hold this data:

```sh
sudo tune2fs -m 0 /dev/disk/by-id/ata-EXAMPLE
```

Mount with `noatime` if you would rather the filesystem did not update
an access time on every scrub. The node does not require that option.

Repeat for each disk. The filesystems do not have to be the same size,
and the machines do not have to match. A new empty disk starts receiving
shards as soon as it is added, because placement prefers free space.

## One server

`/etc/djbod/node.toml` on that machine:

```toml
node_id = "5f0c1d2e-8a9b-4c3d-9e1f-2a3b4c5d6e7f"   # uuidgen, once, for the life of the machine
listen = "10.0.0.1:5263"
state_dir = "/var/lib/djbod"
devices = [
  "/mnt/disk0/djbod",
  "/mnt/disk1/djbod",
  "/mnt/disk2/djbod",
  "/mnt/disk3/djbod",
]
```

If the process should listen on every interface, set `listen` to
`0.0.0.0:5263` and `advertise` to `10.0.0.1:5263`. The document records
`advertise`.

```sh
sudo djbod-node init-cluster --config /etc/djbod/node.toml --name home-nas --k 3 --m 1
sudo djbod-node run --config /etc/djbod/node.toml
```

Four disks and `3+1` means a write can succeed immediately, and you
have no spare disk to drain onto. Add a fifth disk before you drain
one, or pick `2+1` if four bays is all you have and you want to be able
to drain. [Add a device](scenarios.md#add-a-device) is the procedure
either way.

Label the disks once you have the ids from `init-cluster` or from
`djbod status`:

```sh
export DJBOD_NODE=10.0.0.1:5263
export DJBOD_CLUSTER=$(djbod get-cluster-id)
djbod cluster set-node-label <node-uuid> nas1
djbod cluster set-label <device-uuid> nas1-bay0
```

## Three servers

Use the same file on each machine. `node_id`, `listen`, and
`bootstrap_peers` differ. On `nas1` (`10.0.0.1`), with three disks:

```toml
node_id = "5f0c1d2e-8a9b-4c3d-9e1f-2a3b4c5d6e7f"
listen = "10.0.0.1:5263"
state_dir = "/var/lib/djbod"
devices = ["/mnt/disk0/djbod", "/mnt/disk1/djbod", "/mnt/disk2/djbod"]
bootstrap_peers = ["10.0.0.2:5263", "10.0.0.3:5263"]
```

Create the cluster on the first machine only.

```sh
djbod-node init-cluster --config /etc/djbod/node.toml --name home-nas --k 4 --m 2
djbod-node run --config /etc/djbod/node.toml
```

`4+2` needs six devices. The first machine has three, so writes fail
with `InsufficientDevices` until the others have joined. That refusal
stores nothing. It is the expected state of a half-built cluster, not
a fault in the disks.

On `nas2` and `nas3`, with their own `node_id`, `listen`, and device
paths, and with the first node running:

```sh
djbod-node join --config /etc/djbod/node.toml \
  --peer 10.0.0.1:5263 --cluster <cluster-id>
djbod-node run --config /etc/djbod/node.toml
```

`join` fetches the document, initialises each empty device directory,
and proposes a new document version that lists this node and its
devices. Every node already in the cluster has to accept it. The
command prints the new document version and the new device ids, and
tells you to start the node with the same config file.

From an administration machine:

```sh
export DJBOD_NODE=10.0.0.1:5263,10.0.0.2:5263,10.0.0.3:5263
export DJBOD_CLUSTER=$(djbod get-cluster-id)
djbod cluster show
djbod status
djbod put backups/2026-09.tar /path/to/backup.tar
```

`cluster show` should list three nodes at the same document version,
each with a build string. `status` should list nine devices, all
`active`. A `put` then a `get` from a second address confirms that any
node can serve the object:

```sh
djbod --node 10.0.0.2:5263 get backups/2026-09.tar /tmp/copy.tar
```

Label nodes and devices before you need them in an incident. `status`
is much easier to read with `nas2-bay1` than with a UUID, and the
scenarios use those labels.

Adding a fourth server later is `join` again. Adding a disk to a server
that is already a member is [Add a device](scenarios.md#add-a-device),
which is a different command and requires a restart of that node.

## Keep the node running

The repository does not ship a service unit. This is a unit that runs
the binary you installed as `/usr/local/bin/djbod-node`, as a user that
can read the config, the certificates, and the device directories.
Standard error goes to the journal.

```ini
[Unit]
Description=Distributed-JBOD node
After=network-online.target local-fs.target
Wants=network-online.target

[Service]
User=djbod
Group=djbod
ExecStart=/usr/local/bin/djbod-node run --config /etc/djbod/node.toml
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

The device filesystems must be mounted before the process starts. If a
mount is missing, the node still starts and that device is unavailable.
That is the right behaviour for a disk that failed. It is also what you
see when `fstab` has not run yet, so order the unit after the mounts.

At startup the node asks each `bootstrap_peers` entry for a newer
document and adopts it. A machine that was off during a membership
change catches up this way. If its own `listen` or `advertise` no longer
matches the address in the document, it proposes the configured address
before it serves, and it refuses to start when the other nodes cannot
accept that change. [A node moved address](scenarios.md#a-node-moved-address)
is that procedure on purpose.

## An administration machine

Install `djbod` where you will run repairs. Point it at every node, in
an order you like. The first one that answers is used, and a later
command can pass `--node` to pin a single address.

```sh
export DJBOD_NODE=10.0.0.1:5263,10.0.0.2:5263,10.0.0.3:5263
export DJBOD_CLUSTER=<cluster-id>
```

Put the web UI on one machine, bound to localhost, and reach it with an
SSH tunnel (`ssh -L 5264:127.0.0.1:5264 nas1`) or from the machine
itself. The UI has no login of its own. [Day to day](day-to-day.md#the-web-ui)
has the details. Turn on [TLS](tls.md) before any of these addresses are
reachable from a network you do not trust. Until then the protocol is
plain TCP and any client that can connect can do everything, including
administration.

## Upgrading the binaries

`djbod cluster show` prints each node's build in the `BUILD` column.
`djbod status` prints the build of the node that answered, and the
client's own build in parentheses when the two differ. Upgrade every
node before a document change that uses a field the older build does
not understand. A node refuses a document it cannot represent, and the
error names the node. The system does not keep a mixed-version feature
silently by dropping fields.
