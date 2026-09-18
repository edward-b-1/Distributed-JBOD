# Getting started on one machine

This walks through building the binaries, creating a cluster of one node
with four "devices" that are really directories, and storing and fetching
objects with the `djbod` client. Everything here has been run as written.

## Prerequisites

- Linux. The node uses `fallocate` and `statvfs`, and ext4 is the tested
  filesystem; directories on any filesystem that supports `fallocate`
  (ext4, XFS, tmpfs) work for experiments.
- A Rust toolchain. If `cargo --version` fails, install one with
  [rustup](https://rustup.rs):

  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source ~/.cargo/env
  ```

## Build

```sh
git clone https://github.com/edward-b-1/Distributed-JBOD.git
cd Distributed-JBOD
cargo build --release
```

This produces two binaries:

- `target/release/djbod-node`: the node process.
- `target/release/djbod`: the client.

Running `cargo test --workspace` first is a good check of the machine; it
starts nodes on localhost ports and takes a few seconds.

## Configure a node

Pick a base directory and create the device directories and a state
directory. Each device must be **empty** when the cluster is created.

```sh
mkdir -p /tmp/djbod/state /tmp/djbod/d0 /tmp/djbod/d1 /tmp/djbod/d2 /tmp/djbod/d3
```

Write the node's configuration file. It can live anywhere; the node is
told where with `--config` and never looks for it elsewhere. For this
walkthrough put it beside the state directory, at `/tmp/djbod/node.toml`.
On a real installation `/etc/djbod/node.toml` is the natural place, with
the state directory under `/var/lib/djbod`.

```toml
node_id = "6a2d5c1e-3f0b-4b1a-9d2e-0c7e8a9b1f22"   # any UUID; `uuidgen` or /proc/sys/kernel/random/uuid
listen = "127.0.0.1:5263"
state_dir = "/tmp/djbod/state"
devices = ["/tmp/djbod/d0", "/tmp/djbod/d1", "/tmp/djbod/d2", "/tmp/djbod/d3"]

# Only because these four directories are on one disk. On real hardware
# each device is its own filesystem and this line is omitted; the node
# then refuses to start if two devices share a disk.
allow_shared_filesystem = true
```

## Create the cluster

```sh
target/release/djbod-node init-cluster --config /tmp/djbod/node.toml --k 3 --m 1
```

`--k 3 --m 1` means every object is split into three data shards plus one
parity shard, so four devices are needed and any one may be lost. The
default block size is 1 MiB. The command initialises the four directories
(each gets a `DISTRIBUTED-JBOD-DEVICE.json` and an `objects/` tree) and
prints the ids:

```
cluster 4e9a31f4-e921-4767-8477-250de7f1640f created
node    15dd0194-0167-4177-955b-2064ee6fd071
device  2bb98674-...  /tmp/djbod/d0
...
```

`k + m` may be less than the number of devices; that is the normal case.
Each object is placed on the `k + m` devices with the most free space at
the time it is written, so a larger pool is used evenly and different
objects land on different subsets. `k + m` may also be more than the
number of devices, and `init-cluster` allows it because a cluster can
grow, but it warns, and every `put` fails with `InsufficientDevices`
until enough devices exist.

You need the **cluster id** for the client. If you lose it, it is in
`/tmp/djbod/state/cluster.json`.

The warning about devices sharing a filesystem is expected here.

## Run the node

In one terminal:

```sh
target/release/djbod-node run --config /tmp/djbod/node.toml
```

It logs one `node running` line and waits. Stop it with Ctrl-C. To see
every request, run it with `RUST_LOG=debug`; for one JSON object per line,
add `--log-format json`.

## Use the client

In another terminal:

```sh
export DJBOD_NODE=127.0.0.1:5263
export DJBOD_CLUSTER=4e9a31f4-e921-4767-8477-250de7f1640f   # yours

target/release/djbod status
```

```
cluster   4e9a31f4-...
document  version 1
answered  by node 15dd0194-...

DEVICE                                NODE                                  STATE          TOTAL          FREE
2bb98674-...                          15dd0194-...                          active      22.5 GiB      13.2 GiB
...
```

Store, list, inspect, fetch, and delete:

```sh
head -c 3000000 /dev/urandom > cat.jpg
target/release/djbod put photos/cat.jpg cat.jpg --content-type image/jpeg
target/release/djbod list --prefix photos/
target/release/djbod head photos/cat.jpg
target/release/djbod get photos/cat.jpg copy.jpg && cmp cat.jpg copy.jpg && echo identical
echo hello | target/release/djbod put notes/hello.txt -
target/release/djbod get notes/hello.txt          # to standard output
target/release/djbod delete notes/hello.txt
target/release/djbod head notes/hello.txt         # NotFound, exit code 1
```

Add `--json` before the subcommand for machine-readable output, for
example `djbod --json head photos/cat.jpg`.

## What is on disk

Every device holds one shard of each object and a copy of its record:

```
/tmp/djbod/d0/
  DISTRIBUTED-JBOD-DEVICE.json
  objects/default/9c/47/9c4712...79ec/
    01M2TMZW6P3XQ5A0JB7KS55GYE.meta.json    the record, readable JSON
    01M2TMZW6P3XQ5A0JB7KS55GYE.0.shard      shard 0 of that version
```

The directory name is the SHA-256 of the key; the record inside names the
key in plain text, so `grep -r photos/cat.jpg /tmp/djbod` finds it. The
shard file is a 4 KiB header, the blocks, and a footer with the checksum
table (SPEC.md 9.3.2).

## Things to try

**Bitrot.** Flip one byte inside a shard's data and read the object back:

```sh
f=$(ls /tmp/djbod/d1/objects/default/*/*/*/*.shard | head -1)
printf '\x01' | dd of="$f" bs=1 seek=$((4096 + 100)) conv=notrunc 2>/dev/null
target/release/djbod get photos/cat.jpg copy.jpg
```

The read fails and names the device, shard, and stripe. This is the
fail-stop behaviour of version 1: reads report damage rather than healing
it. `head` still works, because the records are intact. To fix it:

```sh
target/release/djbod repair photos/cat.jpg
```

Repair reads every shard, rebuilds the damaged one from the other three,
verifies the whole object against the record, and rewrites the damaged
file. The report names each shard's condition and what was rewritten.
`get` then works again.

Use `dd` or `printf` as above to damage a file in place. A text editor
will save it with a trailing newline or re-encoded bytes, which changes
its length; the node reports that as a trailer that does not describe the
file, and `repair` fixes it just the same.

**Finding damage before a client does.** Repair fixes what a read has
tripped over; a scrub finds damage first. Today the check is a
single-machine, offline tool:

```sh
target/release/djbod-node scrub --config /tmp/djbod/node.toml
```

It reads every record and every block on this machine's devices against
their checksums and prints one line per finding, exit code 2 if anything
is wrong, 0 if clean. It reads the disks directly and needs no running
node. `--rate-mib 50` caps the read rate, and `--json` gives one object
per finding. Fix what it finds with `djbod repair <key>`.

The cluster-wide scrub, `djbod scrub`, which sweeps every node from one
command and repairs what it finds, arrives with multi-node support.

**A missing shard.** Delete a `.shard` file and `get` fails with
`NotFound` naming the device that lost it; `repair` recreates the file.

**Too much damage.** Damage two shards of a 3+1 object and `repair`
refuses, naming the stripe where only two of the three needed blocks were
usable, and changes nothing.

**A damaged record.** Edit a number in one `.meta.json` and `head` fails
with `RecordsInconsistent`, because the record's own checksum no longer
matches (SPEC.md 9.4.5).

**Replace.** `put` the same key twice and look at the device directory:
only the newest version's files remain.

**Not enough devices.** Create a cluster with `--k 3 --m 1` on three
devices and `put` fails with `InsufficientDevices` before writing
anything.

## A second node

Nodes normally run on different machines, one per machine. To try it on
one machine, give the second node its own port, state directory, and
devices, and point it at the first as a bootstrap peer:

```toml
# /tmp/djbod2/node.toml
node_id = "<another UUID>"
listen = "127.0.0.1:5264"
state_dir = "/tmp/djbod2/state"
devices = ["/tmp/djbod2/d0", "/tmp/djbod2/d1"]
bootstrap_peers = ["127.0.0.1:5263"]
allow_shared_filesystem = true
```

With the first node running, join and start the second:

```sh
target/release/djbod-node join --config /tmp/djbod2/node.toml     --peer 127.0.0.1:5263 --cluster $DJBOD_CLUSTER
target/release/djbod-node run --config /tmp/djbod2/node.toml
```

`join` fetches the cluster document from the peer, initialises the new
devices, and proposes a new document version listing the node and its
devices; every existing node must accept it. Then:

```sh
target/release/djbod cluster show          # every node and the document version it holds
target/release/djbod put big/file some.bin  # shards now land on both nodes
target/release/djbod --node 127.0.0.1:5264 get big/file copy.bin   # any node serves any object
```

Stop one node and any request needing it fails naming the node, then
works again when it is back. If a document change ever reaches some nodes
and not others, `djbod cluster show` shows the versions disagreeing and
`djbod cluster sync` brings every reachable node up to the highest.

To give an existing node another device, list the new path in its
configuration, run `djbod-node add-device --config <file> --path <the
path>`, and restart the node.

On real machines, `listen` is that machine's own address, or `0.0.0.0`
with `advertise` set to the address the others should use, and
`allow_shared_filesystem` is omitted.

## Starting over

Stop the node and remove `/tmp/djbod`. `init-cluster` refuses a
non-empty device directory, so a fresh cluster needs fresh directories.
