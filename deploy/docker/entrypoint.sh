#!/bin/sh
# Entry point of the djbod image: settle this node's id, create or join a
# cluster the first time the state directory has no document, then run
# the node. Every setting comes from the DJBOD_* environment (SPEC 20.6);
# nothing needs choosing in advance.
#
#   DJBOD_NODE_ID       optional: this node's UUID. Generated on the first
#                       start and kept in the state directory otherwise,
#                       so it is stable for the node's life either way.
#   DJBOD_ADVERTISE     required unless --network host: the IP:port other
#                       nodes and clients use to reach this container
#   DJBOD_DEVICES       device paths, comma-separated (default /data/d0,/data/d1)
#   DJBOD_STATE_DIR     state directory (default /var/lib/djbod)
#   DJBOD_JOIN_PEER     set on a joining node: IP:port of a running node,
#                       which is asked for the cluster id (SPEC 19.1.5.1)
#   DJBOD_K, DJBOD_M    the scheme, first node only (default 3+1)
#   DJBOD_CLUSTER_NAME  a name shown beside the cluster id, first node only;
#                       `djbod cluster set-name` changes it later
#   DJBOD_BOOTSTRAP_PEERS, DJBOD_TLS_*, and the rest as for djbod-node
set -eu

# A variable set to nothing, as a compose file does for one it does not
# use, is the same as one not set.
for name in DJBOD_NODE_ID DJBOD_ADVERTISE DJBOD_JOIN_PEER DJBOD_BOOTSTRAP_PEERS DJBOD_CLUSTER_NAME; do
    eval "value=\${$name:-}"
    [ -n "$value" ] || unset "$name"
done

state="${DJBOD_STATE_DIR:-/var/lib/djbod}"
if [ -z "${DJBOD_NODE_ID:-}" ]; then
    if [ -f "$state/node-id" ]; then
        DJBOD_NODE_ID=$(cat "$state/node-id")
    else
        DJBOD_NODE_ID=$(cat /proc/sys/kernel/random/uuid)
        printf '%s\n' "$DJBOD_NODE_ID" > "$state/node-id"
        echo "djbod-entrypoint: this node's id is $DJBOD_NODE_ID, kept in $state/node-id" >&2
    fi
    export DJBOD_NODE_ID
fi

if [ ! -f "$state/cluster.json" ]; then
    if [ -n "${DJBOD_JOIN_PEER:-}" ]; then
        # The peer says which cluster it serves; it may still be starting.
        until cluster=$(djbod get-cluster-id --node "$DJBOD_JOIN_PEER" 2>/dev/null); do
            echo "djbod-entrypoint: $DJBOD_JOIN_PEER does not answer yet; retrying in 3 seconds" >&2
            sleep 3
        done
        # Join is idempotent, so a failure part way is retried whole.
        until djbod-node join --peer "$DJBOD_JOIN_PEER" --cluster "$cluster"; do
            echo "djbod-entrypoint: join failed; retrying in 3 seconds" >&2
            sleep 3
        done
    else
        djbod-node init-cluster --k "${DJBOD_K:-3}" --m "${DJBOD_M:-1}"
    fi
fi
exec djbod-node run "$@"
