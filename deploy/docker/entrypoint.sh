#!/bin/sh
# Entry point of the djbod image: create or join a cluster the first time
# the state directory is empty, then run the node. Every setting comes from
# the DJBOD_* environment (SPEC 20.6).
#
#   DJBOD_NODE_ID       required: this node's UUID, fixed for its life
#   DJBOD_ADVERTISE     required unless --network host: the IP:port other
#                       nodes and clients use to reach this container
#   DJBOD_DEVICES       device paths, comma-separated (default /data/d0,/data/d1)
#   DJBOD_STATE_DIR     state directory (default /var/lib/djbod)
#   DJBOD_CLUSTER_ID    the cluster id: chosen for the first node, required
#                       for a joining one
#   DJBOD_JOIN_PEER     set on a joining node: IP:port of a running node
#   DJBOD_K, DJBOD_M    the scheme, first node only (default 3+1)
#   DJBOD_CLUSTER_NAME  a name shown beside the cluster id, first node only;
#                       `djbod cluster set-name` changes it later
#   DJBOD_BOOTSTRAP_PEERS, DJBOD_TLS_*, and the rest as for djbod-node
set -eu

state="${DJBOD_STATE_DIR:-/var/lib/djbod}"
if [ ! -f "$state/cluster.json" ]; then
    if [ -n "${DJBOD_JOIN_PEER:-}" ]; then
        : "${DJBOD_CLUSTER_ID:?DJBOD_CLUSTER_ID is required to join a cluster}"
        # The peer may still be starting; join is idempotent, so retry.
        until djbod-node join --peer "$DJBOD_JOIN_PEER" --cluster "$DJBOD_CLUSTER_ID"; do
            echo "djbod-entrypoint: join failed; retrying in 3 seconds" >&2
            sleep 3
        done
    else
        djbod-node init-cluster --k "${DJBOD_K:-3}" --m "${DJBOD_M:-1}"
    fi
fi
exec djbod-node run "$@"
