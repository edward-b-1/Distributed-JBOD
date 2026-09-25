# Setup

This builds the binaries and makes a one-machine cluster whose devices
are directories on a single disk. That cluster is for learning the
commands. `allow_shared_filesystem` is set so the node will start, and
it means the redundancy is not real: one disk failure takes every
directory with it. [Deployment](deployment.md) is the same sequence on
one filesystem per disk, without that flag.

## What you need

Linux, and a filesystem that supports `fallocate`. The node allocates
each shard file with `fallocate`. ext4 and XFS do this. A Rust toolchain
is required to build from source. If `cargo --version` fails:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Build

```sh
git clone https://github.com/edward-b-1/Distributed-JBOD.git
cd Distributed-JBOD
cargo build --release
```

The binaries are:

- `target/release/djbod-node`
- `target/release/djbod`
- `target/release/djbod-ui`
- `target/release/djbod-recover`

`cargo test --workspace` starts nodes on localhost ports and is a useful
check that the machine can run the software. It does not need the
directories you are about to create.

## One node, four directories

Pick a directory that is not already a device. The paths below are an
example. Each device directory must exist and be empty. `init-cluster`
refuses a directory that contains anything other than, optionally,
`lost+found`.

```sh
mkdir -p /var/tmp/djbod-trial/state \
  /var/tmp/djbod-trial/d0 /var/tmp/djbod-trial/d1 \
  /var/tmp/djbod-trial/d2 /var/tmp/djbod-trial/d3
uuidgen
```

Write `/var/tmp/djbod-trial/node.toml`. Use the UUID `uuidgen` printed.
The node reads only the file you pass to `--config`. It does not search
for a default path.

```toml
node_id = "6a2d5c1e-3f0b-4b1a-9d2e-0c7e8a9b1f22"
listen = "127.0.0.1:5263"
state_dir = "/var/tmp/djbod-trial/state"
devices = [
  "/var/tmp/djbod-trial/d0",
  "/var/tmp/djbod-trial/d1",
  "/var/tmp/djbod-trial/d2",
  "/var/tmp/djbod-trial/d3",
]
allow_shared_filesystem = true
```

`listen` is the address this process binds. Use `127.0.0.1` for a trial
so it is not reachable from the network. Omit `allow_shared_filesystem`
on real hardware. Without it, two device paths on the same filesystem
are refused:

```text
devices /path/a and /path/b are on the same filesystem; two configured devices must be two disks
```

Create the cluster. `--k 2 --m 1` needs three devices and survives one
lost shard. Four directories leave one spare, which you will want as
soon as you try to drain a disk. The defaults, if you omit `--k` and
`--m`, are `3+1`.

```sh
target/release/djbod-node init-cluster \
  --config /var/tmp/djbod-trial/node.toml \
  --name trial --k 2 --m 1
```

The command prints the cluster id, the node id, and one line per device:

```text
cluster 1f255ad2-7974-4703-9121-55105b91d486 created
node    f85f5a44-ca2c-4456-8ab3-6e8a74a9ebf9
device  948f2137-26c3-4969-996f-494c50fb2313  /var/tmp/djbod-trial/d0
...
```

Keep the cluster id. It is also in
`/var/tmp/djbod-trial/state/cluster.json`, and any running node repeats
it to `djbod get-cluster-id`.

If you create a cluster with fewer active devices than `k + m`,
`init-cluster` still succeeds and logs a warning. Every `put` then fails
with `InsufficientDevices` until more devices have joined. A trial of
`--k 3 --m 1` on two directories does that: the node starts, and the
first `put` is refused before it writes.

## Run the node

```sh
target/release/djbod-node run --config /var/tmp/djbod-trial/node.toml
```

It logs one `node running` line to standard error and waits. Stop it
with Ctrl-C. `RUST_LOG=debug` prints every request. `--log-format json`
prints one JSON object per line, still on standard error. The process
does not write a log file. A service manager that captures standard
error is the log.

With `allow_shared_filesystem` set, the log includes a warning that
those devices share one filesystem and that losing that disk loses all
of them.

## The client

In another terminal:

```sh
export DJBOD_NODE=127.0.0.1:5263
export DJBOD_CLUSTER=$(target/release/djbod get-cluster-id)
target/release/djbod identity
target/release/djbod status
```

`identity` needs only `DJBOD_NODE`. It prints the cluster name and id,
the node, the build, the document version, and the transport.
`get-cluster-id` prints the UUID alone, which is why the `export` above
works. `--json` on `get-cluster-id` adds the name and the build.

`status` needs both variables. It prints who answered, then one row per
device: id, label, owning node, that node's build, state, filesystem
total, and free space after headroom. Several addresses may be given,
comma-separated. They are tried in order, and a request moves to the
next when one fails:

```sh
export DJBOD_NODE=127.0.0.1:9,127.0.0.1:5263
```

Port 9 is closed. The client reports the failure and uses `5263`.

Every `djbod` command accepts the same values as flags: `--node` and
`--cluster`. A flag wins over the environment variable.

## Store and fetch

```sh
echo hello > /tmp/hello.txt
target/release/djbod put notes/hello.txt /tmp/hello.txt --content-type text/plain
echo piped | target/release/djbod put notes/piped.txt -
head -c 20000 /dev/urandom > /tmp/cat.bin
target/release/djbod put photos/cat.bin /tmp/cat.bin --content-type application/octet-stream

target/release/djbod list
target/release/djbod list --prefix photos/
target/release/djbod head photos/cat.bin
target/release/djbod get photos/cat.bin /tmp/cat.out
cmp /tmp/cat.bin /tmp/cat.out && echo identical
target/release/djbod get notes/hello.txt
target/release/djbod delete notes/piped.txt
target/release/djbod head notes/piped.txt
```

`put` prints `stored <key> as version <id>`. The file argument `-` reads
standard input, and it has to read all of it first because the size is
sent up front. `get` writes to the path you give, or to standard output
when the path is `-` or omitted. `list` prints the size, the version id,
and the key. `--limit` and `--start-after` page through a large listing;
the server also stops a page at 8 MiB of key text and tells you to pass
`--start-after`. `head` prints the size, the checksum, the scheme, the
content type, and the device that holds each shard. `delete` prints
`deleted <key>`. A missing key is `NotFound` and exit code 1.

`djbod --json <command>` prints the same facts as one JSON value. Put
`--json` before the subcommand.

`djbod contents` counts versions, keys, blocks, and shard bytes from the
records, without reading the payload. A device with zero versions holds
nothing. That is the check you use before removing a disk, because free
space includes the filesystem's own overhead and will never be the whole
disk.

## Labels

UUIDs are what the files store. Labels are what you type after the first
day.

```sh
target/release/djbod cluster set-node-label <node-uuid> trial
target/release/djbod cluster set-label <device-uuid> bay0
target/release/djbod status
```

From then on `bay0` and `trial` work wherever a command takes a device
or a node. `set-label <name> --clear` and `set-node-label <name> --clear`
remove them. `cluster set-name` changes the cluster's display name. The
id clients send stays the same.

## What is on a device

```text
/var/tmp/djbod-trial/d0/
  DISTRIBUTED-JBOD-DEVICE.json
  objects/default/<aa>/<bb>/<sha256-of-key>/
    <version>.meta.json
    <version>.<shard-index>.shard
```

The identity file is JSON: `system` is `distributed-jbod`, `device_id`
and `cluster_id` are UUIDs, `format_version` is the on-disk format, and
`notice` says the directory is managed by the node and should not be
edited by hand. The directory name under `objects/` is the SHA-256 of
the key. The `.meta.json` file next to the shard is the record, and it
contains the key in plain text, so `grep -r notes/hello.txt
/var/tmp/djbod-trial` finds it. The record also lists the scheme, the
size, the checksum, and the device id of every shard.

The node's state directory holds `cluster.json`, the copy of the cluster
document. It is the membership, not the objects.

Do not edit these files to fix a problem. The scenarios that follow use
the commands. The one exception is a single record file that has become
unreadable JSON, and [that scenario](scenarios.md#a-record-file-is-unreadable)
says exactly which file, because `repair` names it and then stops.

## The web UI on a trial

```sh
target/release/djbod-ui --listen 127.0.0.1:5264
```

It uses `DJBOD_NODE` and `DJBOD_CLUSTER`. Open `http://127.0.0.1:5264/`.
The page has no login. Leave it on localhost. [Day to day](day-to-day.md#the-web-ui)
describes what the page can and cannot do.

## Throw the trial away

Stop the node and remove the directory you created. `init-cluster` will
not reuse a device directory that already has an identity file. A disk
the cluster has removed is a separate case, covered with
`--wipe-removed-device` in [Scenarios](scenarios.md#reusing-a-disk-the-cluster-has-removed).

## Configuration reference

Every node setting can be given in the TOML file, as a flag, or as an
environment variable. A flag wins over a variable, which wins over the
file. The file may be omitted when the required settings come from the
other two.

| TOML | Flag | Environment |
|---|---|---|
| `node_id` | `--node-id` | `DJBOD_NODE_ID` |
| `listen` | `--listen` | `DJBOD_LISTEN` |
| `advertise` | `--advertise` | `DJBOD_ADVERTISE` |
| `state_dir` | `--state-dir` | `DJBOD_STATE_DIR` |
| `devices` | `--device` (repeatable) | `DJBOD_DEVICES` (comma-separated) |
| `bootstrap_peers` | `--bootstrap-peer` (repeatable) | `DJBOD_BOOTSTRAP_PEERS` |
| `temporary_max_age_secs` | `--temporary-max-age-secs` | `DJBOD_TEMPORARY_MAX_AGE_SECS` |
| `stream_idle_timeout_secs` | `--stream-idle-timeout-secs` | `DJBOD_STREAM_IDLE_TIMEOUT_SECS` |
| `allow_shared_filesystem` | `--allow-shared-filesystem` | `DJBOD_ALLOW_SHARED_FILESYSTEM` |
| `[tls]` `cert`, `key`, `ca` | `--tls-cert`, `--tls-key`, `--tls-ca` | `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, `DJBOD_TLS_CA` |

The file path itself is `--config` / `DJBOD_CONFIG`.

`listen` defaults to `0.0.0.0:5263` when the file omits it. Set it
explicitly. When `listen` is a wildcard, set `advertise` to the address
other nodes should dial. The cluster document stores the advertised
address, not the wildcard.

`bootstrap_peers` is consulted at startup. A node that was off while the
document changed adopts a newer copy from a peer. The first node, the
one you ran `init-cluster` on, can leave the list empty until other
machines exist. After that, list the other machines.

`temporary_max_age_secs` defaults to 3600. Temporary files older than
that are deleted when the node starts. `stream_idle_timeout_secs`
defaults to 120. A body or shard stream that delivers nothing for that
long is abandoned.

`init-cluster` also takes the scheme and the limits. Defaults are `k` 3,
`m` 1, block size 1048576, headroom 0.05, maximum key 16384 bytes,
maximum object 1 TiB (`1099511627776`), maximum user metadata 10 MiB
(`10485760`). `--name` is optional. User metadata is carried by the
Rust and Python clients. `djbod put` records a content type and does not
attach metadata of its own. The metadata limit still applies to clients
that set it.

`djbod cluster-config` prints the document as JSON, which is the way to
see the limits, the headroom, and the transport that are in force.
