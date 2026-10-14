#!/bin/bash
set -eu
node_id=$1
bin=/home/rocky/tape-rs-xattr-ha-20260924/ltfsd
if [ "${2:-}" = bootstrap ]; then
 bin=/home/rocky/tape-rs-xattr-ha-20260924/ha-bootstrap
 export TAPE_RS_INTEROP_POOL_UUID=0ba1b026-9595-4b86-9056-c54c7858e744
fi
exec sudo env TAPE_RS_INTEROP_POOL_UUID="${TAPE_RS_INTEROP_POOL_UUID:-}" RUST_LOG=info "$bin" --id "$node_id" --listen 0.0.0.0:7500 --peer 1=10.131.9.71:7500 --peer 2=10.131.9.72:7500 --peer 3=10.131.9.74:7500 --data-dir /home/rocky/tape-rs-xattr-ha-20260924/test-data --changer-serial IBMtaper2287_LL3 --drive-serial IBMtaper2287 --drive-serial IBMtaper3D5E --interval-ms 1000 --client-listen 0.0.0.0:7501 --client-port 7501 --batch-idle-ms 500 --batch-max-wait-ms 5000 --read-idle-ms 15000
