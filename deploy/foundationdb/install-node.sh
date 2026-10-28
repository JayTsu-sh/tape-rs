#!/usr/bin/env bash
set -euo pipefail

stage=${1:?指定已传输的临时目录}
test "$(id -u)" -eq 0
test -f "$stage/fdb.cluster"
test -x "$stage/fdbserver"
test -f "$stage/libfdb_c.so"
test -f "$stage/tape-rs-fdb@.service"
echo '1ddd719d00422cff578c8df75d232afa466556b9ded5ef8b7714dc47562be5d6  '"$stage/fdbserver" | sha256sum --check --status
echo '1be5d328d48b7e032eeb5fefd64e01cdb0b4978367bf1480954421c9fb721d4b  '"$stage/libfdb_c.so" | sha256sum --check --status

if ! getent passwd tape-fdb >/dev/null; then
    useradd --system --home-dir /var/lib/tape-rs-fdb --shell /sbin/nologin tape-fdb
fi
install -d -o root -g root -m 0755 /usr/local/libexec/tape-rs-fdb
install -d -o tape-fdb -g tape-fdb -m 0750 /etc/tape-rs-fdb /var/lib/tape-rs-fdb /var/log/tape-rs-fdb
install -d -o tape-fdb -g tape-fdb -m 0700 /var/lib/tape-rs-fdb/data
if test -e /usr/local/libexec/tape-rs-fdb/fdbserver; then
    cmp -s /usr/local/libexec/tape-rs-fdb/fdbserver "$stage/fdbserver"
else
    install -o root -g root -m 0755 "$stage/fdbserver" /usr/local/libexec/tape-rs-fdb/fdbserver
fi
if test -e /usr/local/libexec/tape-rs-fdb/libfdb_c.so; then
    cmp -s /usr/local/libexec/tape-rs-fdb/libfdb_c.so "$stage/libfdb_c.so"
else
    install -o root -g root -m 0644 "$stage/libfdb_c.so" /usr/local/libexec/tape-rs-fdb/libfdb_c.so
fi

if test -e /etc/tape-rs-fdb/fdb.cluster; then
    cmp -s /etc/tape-rs-fdb/fdb.cluster "$stage/fdb.cluster"
else
    install -o tape-fdb -g tape-fdb -m 0640 "$stage/fdb.cluster" /etc/tape-rs-fdb/fdb.cluster
fi
install -o root -g root -m 0644 "$stage/tape-rs-fdb@.service" /etc/systemd/system/tape-rs-fdb@.service
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/tape-rs-fdb@.service
