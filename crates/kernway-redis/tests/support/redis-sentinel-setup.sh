#!/usr/bin/env bash
# A Redis Sentinel deployment for the driver's tests: one master (6390), one
# replica (6391) and three sentinels (26390-26392) watching master "kwmaster"
# with quorum 2 — all in ONE container, every port published 1:1, so the
# addresses Sentinel hands out (127.0.0.1:port) work inside the container and
# from the host alike, on Linux, macOS and Windows.
#
#   crates/kernway-redis/tests/support/redis-sentinel-setup.sh
set -euo pipefail
NAME="${REDIS_SENTINEL_CONTAINER:-kernway-redis-sentinel}"
IMAGE="${REDIS_IMAGE:-redis:7.4-alpine}"
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p 6390:6390 -p 6391:6391 -p 26390:26390 -p 26391:26391 -p 26392:26392 \
  "$IMAGE" sh -c '
  redis-server --port 6390 --daemonize yes --replica-announce-ip 127.0.0.1 --save "" --appendonly no
  redis-server --port 6391 --daemonize yes --replicaof 127.0.0.1 6390 --replica-announce-ip 127.0.0.1 --save "" --appendonly no
  for p in 26390 26391 26392; do
    printf "port $p\nsentinel announce-ip 127.0.0.1\nsentinel monitor kwmaster 127.0.0.1 6390 2\nsentinel down-after-milliseconds kwmaster 1000\nsentinel failover-timeout kwmaster 5000\nsentinel parallel-syncs kwmaster 1\n" > /tmp/s$p.conf
    redis-sentinel /tmp/s$p.conf --daemonize yes
  done
  tail -f /dev/null' >/dev/null
# Wait until the sentinels agree on the master and see the replica.
for _ in $(seq 1 60); do
  n=$(docker exec "$NAME" redis-cli -p 26390 SENTINEL REPLICAS kwmaster 2>/dev/null | grep -c 6391 || true)
  s=$(docker exec "$NAME" redis-cli -p 26390 SENTINEL SENTINELS kwmaster 2>/dev/null | grep -c '^ip$' || true)
  if [ "$n" -ge 1 ] && [ "$s" -ge 2 ]; then break; fi
  sleep 1
done
echo "export KERNWAY_REDIS_SENTINEL_URL=redis+sentinel://127.0.0.1:26390,127.0.0.1:26391,127.0.0.1:26392/kwmaster"
