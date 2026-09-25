#!/bin/bash
set -eu
export TAPE_RS_HA_BARCODE=RT2502L8
export TAPE_RS_HA_DIRECT=10.131.9.71:7501,10.131.9.72:7501,10.131.9.74:7501
export TAPE_RS_HA_ENDPOINTS="${2:-$TAPE_RS_HA_DIRECT}"
exec /home/rocky/tape-rs-xattr-ha-20260924/hw_client_failover "$1"
