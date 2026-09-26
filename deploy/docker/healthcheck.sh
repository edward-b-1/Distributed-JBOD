#!/bin/sh
# Container health check: the node in this container answers a status
# request. The cluster id is read from the saved document, so no extra
# variable is needed; before the first start there is no document and the
# check reports unhealthy, which start-period covers.
set -eu
state="${DJBOD_STATE_DIR:-/var/lib/djbod}"
cluster=$(sed -n 's/.*"cluster_id": *"\([^"]*\)".*/\1/p' "$state/cluster.json" 2>/dev/null | head -1)
[ -n "$cluster" ] || exit 1
port="${DJBOD_LISTEN##*:}"
exec djbod --node "127.0.0.1:${port:-5263}" --cluster "$cluster" status > /dev/null
