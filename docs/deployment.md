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
   sudo mkdir -p /etc/djbod /var/lib/djbod && sudo chown djbod:djbod /var/lib/djbod
   ```

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
   sudo -u djbod djbod-node init-cluster --config /etc/djbod/node.toml --k 3 --m 1
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

   The unit runs the node as `djbod`, restarts it on failure, and confines
   it to its state directory and `/srv/djbod` for writing; edit
   `ReadWritePaths=` if your disks are mounted elsewhere. A node that has
   been removed from the cluster (`djbod cluster remove-node`) exits
   cleanly and is not restarted, which is the intended outcome.

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

7. **The web UI**, optionally, from the same `client.env`:

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
each machine in turn. The on-disk format is versioned and every record
carries its format version; a node refuses a document or record it does
not understand rather than guessing.

## Docker

The image in the repository's `Dockerfile` holds all four binaries and
runs the node as an unprivileged user. Build it once:

```sh
docker build -t djbod .
```

The entry point creates or joins a cluster the first time the state
volume is empty, then runs the node, all from environment variables:

| Variable | Meaning |
|----------|---------|
| `DJBOD_NODE_ID` | This node's UUID. Choose it once; keep it for the node's life. |
| `DJBOD_ADVERTISE` | The IP and port other nodes and clients use to reach this container. Not needed with `--network host`, where `DJBOD_LISTEN` is the machine's own address. |
| `DJBOD_DEVICES` | Comma-separated device paths inside the container; default `/data/d0,/data/d1`. Mount one disk on each. |
| `DJBOD_CLUSTER_ID` | The cluster id: chosen for the first node (any UUID), required for a node that joins. |
| `DJBOD_JOIN_PEER` | Set on a joining node: a running node's IP and port. The entry point retries until the peer answers. |
| `DJBOD_K`, `DJBOD_M` | The scheme, read by the first node only; default `3` and `1`. |
| `DJBOD_BOOTSTRAP_PEERS` | Other nodes to consult at startup for a newer cluster document, comma-separated. |
| `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, `DJBOD_TLS_CA` | Paths of mounted TLS material, with the key readable only by uid 5263. |
| `DJBOD_ALLOW_SHARED_FILESYSTEM` | `true` only for experiments where several devices share one disk. |

One node per machine, on the host network so the node's address is the
machine's address:

```sh
docker run -d --name djbod --network host --restart unless-stopped \
  -e DJBOD_NODE_ID=$(uuidgen) \
  -e DJBOD_LISTEN=10.0.0.1:5263 \
  -e DJBOD_CLUSTER_ID=<the cluster id> \
  -e DJBOD_JOIN_PEER=10.0.0.2:5263 \          # omit on the first machine
  -v /var/lib/djbod:/var/lib/djbod \
  -v /mnt/disk0:/data/d0 -v /mnt/disk1:/data/d1 \
  djbod
```

The state directory and the disks are bind mounts owned by uid 5263 (the
`djbod` user in the image); `chown -R 5263:5263` them once. The image's
`djbod` client works from inside the container:

```sh
docker exec djbod djbod --node 10.0.0.1:5263 --cluster <id> status
```

## Docker Compose: three nodes on one machine

`deploy/docker-compose.yml` starts three nodes with two volumes each and
the web UI, on a private network with fixed addresses, and creates a
`2+1` cluster. It is for trying the system out; the six "devices" are on
one disk.

```sh
docker compose -f deploy/docker-compose.yml up -d --build
docker compose -f deploy/docker-compose.yml exec node1 djbod \
  --node 172.28.0.11:5263 --cluster 3d1e7b3a-0c3f-4b0e-9a7f-1a2b3c4d5e6f status
open http://127.0.0.1:5264/
docker compose -f deploy/docker-compose.yml down -v     # deletes the data too
```

Node 1 creates the cluster with the id written in the file; nodes 2 and 3
join it, retrying until node 1 answers, so the order the containers
start in does not matter. Stop and start the stack and every node comes
back with its state. If port 5264 is already in use on the host, publish
the UI elsewhere with `DJBOD_UI_PORT=15264 docker compose ... up -d`.

Everything in the file is ordinary Compose: the fixed addresses exist
because the cluster document holds nodes by IP and port, the UUIDs are
arbitrary and can be changed, and `DJBOD_ALLOW_SHARED_FILESYSTEM` is set
because the volumes share the host's disk.

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
