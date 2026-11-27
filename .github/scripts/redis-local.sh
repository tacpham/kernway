#!/usr/bin/env bash
# Start the Redis topologies the kernway-redis tests use as local processes
# (no Docker), from redis-server / redis-sentinel / redis-cli in PATH (or
# REDIS_SERVER, REDIS_SENTINEL, REDIS_CLI):
#
#   plain     6380, plus a Unix socket in $DIR/redis.sock
#   tls       6381, TLS only, CA / server (DNS:localhost) / client certificates
#   sentinel  master 6390, replica 6391, sentinels 26390-26392 ("kwmaster")
#   cluster   7000-7005: three masters, three replicas
#
#   .github/scripts/redis-local.sh <dir> [modes…]     (default: all four)
#
# The same layouts as tests/support/redis-*-setup.sh (which use Docker).
# Prints the test environment, also to $GITHUB_ENV when set.
set -euo pipefail
DIR="$(mkdir -p "${1:?usage: $0 <dir> [plain tls sentinel cluster]}" && cd "$1" && pwd)"
shift
MODES="${*:-plain tls sentinel cluster}"
SERVER="${REDIS_SERVER:-redis-server}"
SENTINEL="${REDIS_SENTINEL:-redis-sentinel}"
CLI="${REDIS_CLI:-redis-cli}"
COMMON=(--save "" --appendonly no --daemonize yes)
ENV_LINES=()

# Newer Redis 8 refuses a cluster whose bus port has no TLS unless told the
# (loopback-only, test) bus is fine. Older versions do not know the option:
# Redis 7 exits on it, Redis 8.0 daemonizes and then aborts — so probe once
# in the foreground whether this server keeps running with it.
BUS_OPTION=""
bus_protection_option() {
  if [ -z "$BUS_OPTION" ]; then
    # It must listen somewhere (a server with nothing to listen on exits at once).
    "$SERVER" --port 0 --unixsocket "$DIR/probe.sock" --save "" --cluster-bus-port-protected-mode no --logfile "$DIR/probe.log" --daemonize no &
    local pid=$!
    sleep 1
    if kill -0 "$pid" 2>/dev/null; then BUS_OPTION=yes; kill "$pid"; else BUS_OPTION=no; fi
    wait "$pid" 2>/dev/null || true
  fi
  [ "$BUS_OPTION" = yes ]
}

wait_ping() { # args for redis-cli…
  for _ in $(seq 1 50); do
    if "$CLI" "$@" PING 2>/dev/null | grep -q PONG; then return; fi
    sleep 0.2
  done
  echo "redis did not answer: $*" >&2; exit 1
}

for mode in $MODES; do
  case "$mode" in
    plain)
      mkdir -p "$DIR/plain"
      "$SERVER" --port 6380 --unixsocket "$DIR/redis.sock" --unixsocketperm 777 --dir "$DIR/plain" \
        --logfile "$DIR/plain.log" "${COMMON[@]}"
      wait_ping -p 6380
      # The ACL user the pub/sub and cache tests authenticate as.
      "$CLI" -p 6380 ACL SETUSER kernway on '>kernway_pw' '~*' '&*' '+@all' >/dev/null
      ENV_LINES+=("KERNWAY_REDIS_USER=kernway" "KERNWAY_REDIS_PASS=kernway_pw")
      ENV_LINES+=("KERNWAY_REDIS_URL=redis://127.0.0.1:6380" "KW_REDIS_ADDR=127.0.0.1:6380"
                  "KERNWAY_REDIS_ADDR=127.0.0.1:6380" "KERNWAY_REDIS_SOCKET=$DIR/redis.sock")
      ;;
    tls)
      T="$DIR/tls"; mkdir -p "$T"
      ( cd "$T"
        openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.crt 2>/dev/null
        openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
        printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
        openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
        openssl req -newkey rsa:2048 -nodes -subj "/CN=kernway-client" -keyout client.key -out client.csr 2>/dev/null
        printf 'extendedKeyUsage=clientAuth\n' > client.ext
        openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null )
      "$SERVER" --port 0 --tls-port 6381 --tls-cert-file "$T/server.crt" --tls-key-file "$T/server.key" \
        --tls-ca-cert-file "$T/ca.crt" --tls-auth-clients optional --dir "$T" --logfile "$DIR/tls.log" "${COMMON[@]}"
      wait_ping --tls -p 6381 --cacert "$T/ca.crt" --sni localhost
      ENV_LINES+=("KERNWAY_REDIS_TLS_URL=rediss://localhost:6381" "KERNWAY_REDIS_TLS_DIR=$T")
      ;;
    sentinel)
      S="$DIR/sentinel"; mkdir -p "$S"
      "$SERVER" --port 6390 --replica-announce-ip 127.0.0.1 --dir "$S" --logfile "$DIR/master.log" "${COMMON[@]}"
      "$SERVER" --port 6391 --replicaof 127.0.0.1 6390 --replica-announce-ip 127.0.0.1 --dir "$S" \
        --logfile "$DIR/replica.log" "${COMMON[@]}"
      for p in 26390 26391 26392; do
        printf 'port %s\nsentinel announce-ip 127.0.0.1\nsentinel monitor kwmaster 127.0.0.1 6390 2\nsentinel down-after-milliseconds kwmaster 1000\nsentinel failover-timeout kwmaster 5000\nsentinel parallel-syncs kwmaster 1\ndir %s\nlogfile %s\ndaemonize yes\n' \
          "$p" "$S" "$DIR/sentinel-$p.log" > "$S/s$p.conf"
        "$SENTINEL" "$S/s$p.conf"
      done
      for _ in $(seq 1 100); do
        n=$("$CLI" -p 26390 SENTINEL REPLICAS kwmaster 2>/dev/null | grep -c 6391 || true)
        s=$("$CLI" -p 26390 SENTINEL SENTINELS kwmaster 2>/dev/null | grep -c '^ip$' || true)
        if [ "$n" -ge 1 ] && [ "$s" -ge 2 ]; then break; fi
        sleep 0.3
      done
      ENV_LINES+=("KERNWAY_REDIS_SENTINEL_URL=redis+sentinel://127.0.0.1:26390,127.0.0.1:26391,127.0.0.1:26392/kwmaster")
      ;;
    cluster)
      C="$DIR/cluster"
      for p in 7000 7001 7002 7003 7004 7005; do
        mkdir -p "$C/$p"
        node=(--port "$p" --cluster-enabled yes --cluster-config-file "$C/$p/nodes.conf" --cluster-node-timeout 2000
              --cluster-announce-ip 127.0.0.1 --dir "$C/$p" --logfile "$DIR/cluster-$p.log" "${COMMON[@]}")
        if bus_protection_option; then node+=(--cluster-bus-port-protected-mode no); fi
        "$SERVER" "${node[@]}"
        wait_ping -p "$p"
      done
      "$CLI" --cluster create 127.0.0.1:7000 127.0.0.1:7001 127.0.0.1:7002 127.0.0.1:7003 127.0.0.1:7004 127.0.0.1:7005 \
        --cluster-replicas 1 --cluster-yes >"$DIR/cluster-create.log" 2>&1
      for _ in $(seq 1 100); do
        if "$CLI" -p 7000 CLUSTER INFO 2>/dev/null | grep -q 'cluster_state:ok'; then break; fi
        sleep 0.3
      done
      ENV_LINES+=("KERNWAY_REDIS_CLUSTER_URL=redis+cluster://127.0.0.1:7000,127.0.0.1:7001")
      ;;
    *) echo "unknown mode $mode" >&2; exit 2 ;;
  esac
done
for line in "${ENV_LINES[@]}"; do
  echo "$line"
  if [ -n "${GITHUB_ENV:-}" ]; then echo "$line" >> "$GITHUB_ENV"; fi
done
