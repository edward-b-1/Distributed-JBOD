#!/bin/sh
# Entry point of the web UI container: learn the cluster id from the first
# bootstrap node when DJBOD_CLUSTER is not given (SPEC 19.1.5.1), then run
# djbod-ui with whatever arguments follow.
set -eu
if [ -z "${DJBOD_CLUSTER:-}" ]; then
    first="${DJBOD_BOOTSTRAP_NODE%%,*}"
    until DJBOD_CLUSTER=$(djbod get-cluster-id --bootstrap-node "$first" 2>/dev/null); do
        echo "djbod-ui-entrypoint: $first does not answer yet; retrying in 3 seconds" >&2
        sleep 3
    done
    export DJBOD_CLUSTER
fi
exec djbod-ui "$@"
