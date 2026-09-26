# Deploying Distributed-JBOD

Three ways to run nodes for real, after the walkthrough in
[getting-started.md](getting-started.md): as systemd services on each
machine, as a Docker container on each machine, or, for trying the whole
thing out on one computer, as a Docker Compose stack of three nodes and
the web UI. The example files live in [`deploy/`](../deploy).

Whichever way, the shape is the same: one node process per machine, one
data disk per device path, a state directory per node, and every setting
reachable as a configuration file entry, an environment variable, or an
argument (SPEC 20.6). Nodes are addressed by IP and port in the cluster
document, so each node needs a stable IP.

## systemd

One machine, one node, started at boot and restarted if it fails.

1. **Install the binaries** from `cargo build --release`:

   ```sh
   sudo install -m 755 target/release/djbod-node target/release/djbod \
     target/release/djbod-recover target/release/djbod-ui /usr/local/bin/
   sudo useradd --system --home-dir /var/lib/djbod --shell /usr/sbin/nologin djbod
   ```

   The unit creates `/etc/djbod` and `/var/lib/djbod` itself, owned by
   `djbod`, the first time it starts (`ConfigurationDirectory=` and
   `StateDirectory=`); create `/etc/djbod` by hand only if you write the
   configuration before the first start.

2. **Mount the disks** where the node will find them, one filesystem per
   disk (SPEC 20.5), for example under `/srv/djbod/disk0`, and make each
   mount point writable by `djbod`. The node refuses two device paths on
   one filesystem, which is what you want on real hardware.

3. **Write `/etc/djbod/node.toml`** as in the walkthrough, with `listen`
   set to this machine's address (or `0.0.0.0` with `advertise` set to the
   address other machines use), the mount points as `devices`, and
   `state_dir = "/var/lib/djbod"`. Anything you would rather not put in
   the file can go in `/etc/djbod/node.env` instead; see
   `deploy/systemd/node.env.example`.

4. **Create or join the cluster** once, as the `djbod` user so the state
   directory and devices end up owned by it:

   ```sh
   sudo -u djbod djbod-node init-cluster --config /etc/djbod/node.toml --k 3 --m 1 --name home-nas
   # or, on every further machine:
   sudo -u djbod djbod-node join --config /etc/djbod/node.toml --peer 10.0.0.1:5263 --cluster <id>
   ```

5. **Install and start the unit:**

   ```sh
   sudo install -m 644 deploy/systemd/djbod-node.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now djbod-node
   journalctl -u djbod-node -f
   ```

   The unit runs the node as `djbod`, restarts it on failure, and names
   no site paths: `ProtectSystem=full` leaves `/srv`, `/mnt` and `/var`
   writable, so the disks may be mounted anywhere `node.toml` says. A
   disk that is not mounted when the node starts does not stop it: the
   node starts, reports that device unavailable, and writes nothing into
   the empty mount point (SPEC 5.6). To confine the node further, add a
   drop-in with `systemctl edit djbod-node` setting
   `ProtectSystem=strict` and `ReadWritePaths=` for your mounts. A node
   that has been removed from the cluster (`djbod cluster remove-node`)
   exits cleanly and is not restarted, which is the intended outcome.

6. **The scrub timer**, on one machine only, since a scrub is
   cluster-wide whichever node it is pointed at. Write
   `/etc/djbod/client.env` from `deploy/systemd/client.env.example` with
   the node address and cluster id, then:

   ```sh
   sudo install -m 644 deploy/systemd/djbod-scrub.service deploy/systemd/djbod-scrub.timer /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now djbod-scrub.timer
   systemctl list-timers djbod-scrub.timer
   ```

   It runs weekly at a random time within an hour, rate-limited to 100
   MiB/s per node, and reports rather than repairs; add `--repair` to
   `ExecStart=` if you would rather it fixed what it finds unattended.
   The report is in `journalctl -u djbod-scrub`.

7. **The web UI**, optionally, from the same `client.env`, which gives
   it the nodes as `DJBOD_BOOTSTRAP_NODE`:

   ```sh
   sudo install -m 644 deploy/systemd/djbod-ui.service /etc/systemd/system/
   sudo systemctl enable --now djbod-ui
   ```

   It listens on `127.0.0.1:5264` and has no login of its own. Reach it
   over an SSH tunnel, or put it behind a reverse proxy that
   authenticates.

**TLS** under systemd is the three paths in `node.env` or `node.toml`
(`DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, `DJBOD_TLS_CA`), with the key file
owned by `djbod` and mode 600, and the client's three in `client.env`.
Issue the files with `scripts/djbod-pki.sh` as the walkthrough describes.

**Upgrading**: install the new binaries, `systemctl restart djbod-node` on
each machine in turn. Finish every machine before changing the cluster
document: a node refuses a document with a field its build does not know
rather than dropping it (SPEC 6.2.6.4), and the error names the node.
`djbod cluster show` prints each node's build, the same string as
`djbod-node --version`, so a machine that was missed stands out. The
on-disk format is versioned too; a node refuses a record it does not
understand rather than guessing.

## Docker

The image in the repository's `Dockerfile` holds all four binaries and
runs the node as an unprivileged user. Build it once, passing the commit
so the binaries report a build id (`.git` is outside the build context;
without the argument they say `unknown`):

```sh
docker build -t djbod --build-arg DJBOD_GIT_COMMIT=$(git rev-parse --short=9 HEAD) .
docker run --rm --entrypoint djbod-node djbod --version   # djbod-node 0.1.0+<commit>
```

The entry point settles the node's id, creates or joins a cluster the
first time the state directory has no document, then runs the node, all
from environment variables. Nothing is chosen in advance: the node id is
generated on the first start and kept in the state directory, and a
joining node asks its peer for the cluster id (`djbod get-cluster-id`).

| Variable | Meaning |
|----------|---------|
| `DJBOD_NODE_ID` | Optional. This node's UUID; generated and kept in the state directory when not given, and stable for the node's life either way. |
| `DJBOD_ADVERTISE` | The IP and port other nodes and clients use to reach this container. Not needed with `--network host`, where `DJBOD_LISTEN` is the machine's own address. |
| `DJBOD_DEVICES` | Comma-separated device paths inside the container; default `/data/d0,/data/d1`. Mount one disk on each. |
| `DJBOD_JOIN_PEER` | Set on a joining node: a running node's IP and port. The entry point asks it for the cluster id and retries until it answers. |
| `DJBOD_K`, `DJBOD_M` | The scheme, read by the first node only; default `3` and `1`. |
| `DJBOD_CLUSTER_NAME` | A name shown beside the cluster id, read by the first node only; `djbod cluster set-name` changes it later. |
| `DJBOD_BOOTSTRAP_PEERS` | Other nodes to consult at startup for a newer cluster document, comma-separated. |
| `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, `DJBOD_TLS_CA` | Paths of mounted TLS material, with the key readable only by uid 5263. |
| `DJBOD_ALLOW_SHARED_FILESYSTEM` | `true` only for experiments where several devices share one disk. |

One node per machine, on the host network so the node's address is the
machine's address. The image declares no volumes: the state directory
and the disks are bind mounts, one physical disk per device path, so
the redundancy is real. `deploy/docker-compose.node.yml` is this as a
Compose file, with its settings in one env file per machine
(`deploy/docker/node.env.example`); by hand it is:

```sh
docker run -d --name djbod --network host --restart unless-stopped \
  -e DJBOD_LISTEN=10.0.0.1:5263 \
  -e DJBOD_JOIN_PEER=10.0.0.2:5263 \          # omit on the first machine
  -v /var/lib/djbod:/var/lib/djbod \
  -v /mnt/disk0/djbod:/data/d0 -v /mnt/disk1/djbod:/data/d1 \
  djbod
```

The state directory and the disks are owned by uid 5263 (the `djbod`
user in the image); `chown -R 5263:5263` them once. Bind a directory
*inside* each disk's mount point rather than the mount point itself, as
`/mnt/disk0/djbod` above would be if the disk is mounted at
`/mnt/disk0`: if the disk is ever not mounted, the directory does not
exist, Docker refuses to start the container, and nothing is written
into the root filesystem by mistake.

The image has a health check: every 30 seconds it asks the node in the
container for `status`, reading the cluster id from the saved document,
so `docker ps` shows `healthy` once the node answers. The image's `djbod`
client works from inside the container:

```sh
docker exec djbod djbod --bootstrap-node 10.0.0.1:5263 --cluster <id> status
```

## Docker Compose: three nodes on one machine

`deploy/docker-compose.yml` starts three nodes with two volumes each and
the web UI, on a private network with fixed addresses, and creates a
`2+1` cluster. It is for trying the system out and nothing more: the six
"devices" are Docker volumes on one disk, so there is no redundancy in
it. A node on a real machine uses `docker-compose.node.yml` above.

```sh
DJBOD_GIT_COMMIT=$(git rev-parse --short=9 HEAD) \
  docker compose -f deploy/docker-compose.yml up -d --build
docker compose -f deploy/docker-compose.yml exec node1 sh -c \
  'djbod --bootstrap-node 172.28.0.11:5263 --cluster $(djbod get-cluster-id --bootstrap-node 172.28.0.11:5263) status'
open http://127.0.0.1:5264/
docker compose -f deploy/docker-compose.yml down -v     # deletes the data too
```

No id appears in the file. Each node generates its own on first start
and keeps it in its state volume; node 1 creates the cluster, named
`compose trial`; nodes 2 and 3, and the UI, ask node 1 for the cluster
id, retrying until it answers, so the order the containers start in
does not matter, and `docker compose ps` shows each node healthy once
it serves. Stop and start the stack and every node comes back with
its state; every node lists the others as bootstrap peers, so one that
missed a document change while down adopts it at startup.

For a closer simulation of real machines, give each device its own
filesystem: a fixed-size file with `mkfs.ext4 -m 0` on it, loop-mounted,
with the volume bound to a directory inside the mount. Then free space is
real, the same-filesystem check passes without
`DJBOD_ALLOW_SHARED_FILESYSTEM`, and a missing mount stops the container.
That needs root for the mounts and is left out of this compose file. If port 5264 is already in use on the host, publish
the UI elsewhere with `DJBOD_UI_PORT=15264 docker compose ... up -d`.

Everything in the file is ordinary Compose: the fixed addresses exist
because the cluster document holds nodes by IP and port, and
`DJBOD_ALLOW_SHARED_FILESYSTEM` is set because the volumes share the
host's disk.

## Verified

The Dockerfile, the compose stack, and the systemd units in this
directory were exercised as follows before being committed: the image
built; the compose stack came up with three nodes joined into one
cluster at document version 3 and six active devices; a 300 kB object
was stored through node 3 and read back identical; node 2 was restarted
and served the object from its kept state; the UI container served the
page and reported the cluster; and `systemd-analyze verify` accepted the
units. The systemd flow itself was not run end to end on a machine with
systemd as init, since the development machine is not one; follow the
steps above and treat them as the first soak test.
