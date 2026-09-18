# Distributed-JBOD

A distributed object store that aggregates the mismatched disks of several
commodity machines into one durable, bitrot-protected pool. Every node runs
the same process; there is no master.

The design is in [SPEC.md](SPEC.md). Implementation is in Rust; see
Appendix C of the specification for the crate layout and milestones.

## Trying it on one machine

```sh
cargo build --release

# node.toml
#   node_id = "<any UUID>"
#   listen = "127.0.0.1:5263"
#   state_dir = "/tmp/djbod/state"
#   devices = ["/tmp/djbod/d0", "/tmp/djbod/d1", "/tmp/djbod/d2", "/tmp/djbod/d3"]
#   allow_shared_filesystem = true      # only because these are directories on one disk

target/release/djbod-node init-cluster --config node.toml --k 3 --m 1
#   prints the cluster id
target/release/djbod-node run --config node.toml &

export DJBOD_NODE=127.0.0.1:5263 DJBOD_CLUSTER=<the cluster id>
target/release/djbod status
target/release/djbod put photos/cat.jpg ./cat.jpg
target/release/djbod list --prefix photos/
target/release/djbod get photos/cat.jpg ./copy.jpg
```

On real hardware, each entry in `devices` is a directory on its own
filesystem, and `allow_shared_filesystem` is left unset.

## Default port

Nodes listen on TCP port **5263** by default. It spells JBOD on a telephone
keypad (J=5, B=2, O=6, D=3), and IANA lists it as unassigned. The first
candidate, 7400, turned out to be the DDS/RTPS discovery port used by ROS 2,
so it was dropped. See SPEC.md item 6.1.1.
