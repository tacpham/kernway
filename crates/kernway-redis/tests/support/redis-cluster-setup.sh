#!/usr/bin/env bash
# A Redis Cluster for the driver's tests: six nodes on 7000-7005 (three
# masters, three replicas) in ONE container, ports published 1:1 and
# announced as 127.0.0.1, so the addresses the cluster hands out (MOVED /
# ASK, CLUSTER SLOTS) work from the host on every OS.
#
#   crates/kernway-redis/tests/support/redis-cluster-setup.sh
set -euo pipefail
NAME="${REDIS_CLUSTER_CONTAINER:-kernway-redis-cluster}"
IMAGE="${REDIS_IMAGE:-redis:7.4-alpine}"
docker rm -f "$NAME" >/dev/null 2>&1 || true
PORTS=""; for p in 7000 7001 7002 7003 7004 7005; do PORTS="$PORTS -p $p:$p"; done
# shellcheck disable=SC2086
docker run -d --name "$NAME" $PORTS "$IMAGE" sh -c '
  for p in 7000 7001 7002 7003 7004 7005; do
    mkdir -p /data/$p
    redis-server --port $p --cluster-enabled yes --cluster-config-file /data/$p/nodes.conf \
      --cluster-node-timeout 2000 --cluster-announce-ip 127.0.0.1 --appendonly no --save "" \
      --dir /data/$p --daemonize yes
  done
  sleep 1
  yes yes | redis-cli --cluster create 127.0.0.1:7000 127.0.0.1:7001 127.0.0.1:7002 \
    127.0.0.1:7003 127.0.0.1:7004 127.0.0.1:7005 --cluster-replicas 1 >/tmp/create.log 2>&1
  tail -f /dev/null' >/dev/null
for _ in $(seq 1 60); do
  if docker exec "$NAME" redis-cli -p 7000 CLUSTER INFO 2>/dev/null | grep -q 'cluster_state:ok'; then break; fi
  sleep 1
done
echo "export KERNWAY_REDIS_CLUSTER_URL=redis+cluster://127.0.0.1:7000,127.0.0.1:7001"
