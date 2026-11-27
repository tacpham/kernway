#!/usr/bin/env bash
# MongoDB deployments for the driver's tests:
#
#   standalone  127.0.0.1:27100   auth on; root / root_pw, kernway / kernway_pw
#               (SCRAM-SHA-256 + SHA-1), kw_sha1 / sha1_pw (SCRAM-SHA-1 only),
#               kw_sha256 / sha256_pw (SCRAM-SHA-256 only); Unix socket
#               /tmp/mongodb-27017.sock inside the container
#   tls         localhost:27101   requireTLS, server certificate for
#               DNS:localhost only; X.509 user CN=kw_x509 (client.pem)
#   replica set 127.0.0.1:27111-27113  rs0, three members (27111 preferred primary)
#   sharded     127.0.0.1:27120, 27123  two mongos; config RS 27121, shard RS 27122
#
# Every replica-set member and router announces 127.0.0.1 with the port it is
# published on (ports 1:1), so the host and the members see the same names.
#
# Docker (default; MONGO_IMAGE, default mongo:8.0) or native
# (MONGO_BIN=<dir with mongod, mongos, mongosh>; MONGOSH=<path> when
# mongosh lives elsewhere) — the OS's own build on macOS / Windows runners.
#
#   crates/kernway-orm-mongodb/tests/support/mongo-setup.sh [dir] [standalone tls rs sharded]
#
# Prints the test environment (KERNWAY_MONGO_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/mongo}"
shift || true
WHAT="${*:-standalone tls rs sharded}"
IMAGE="${MONGO_IMAGE:-mongo:8.0}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"
has() { case " $WHAT " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
EXE=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
native_path() { if [ -n "$EXE" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# ---- certificates ------------------------------------------------------------
export MSYS_NO_PATHCONV=1
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost/O=kernway/OU=server" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
cat server.crt server.key > server.pem
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw_x509/O=kernway/OU=clients" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null
cat client.crt client.key > client.pem
X509_USER="$(openssl x509 -in client.crt -noout -subject -nameopt RFC2253 | sed 's/^subject=//')"
unset MSYS_NO_PATHCONV
chmod 644 ./*.pem ./*.key

cat > users.js <<'JS'
const a = db.getSiblingDB("admin");
a.createUser({ user: "kernway", pwd: "kernway_pw", roles: ["root"], mechanisms: ["SCRAM-SHA-256", "SCRAM-SHA-1"] });
a.createUser({ user: "kw_sha1", pwd: "sha1_pw", roles: ["root"], mechanisms: ["SCRAM-SHA-1"] });
a.createUser({ user: "kw_sha256", pwd: "sha256_pw", roles: ["root"], mechanisms: ["SCRAM-SHA-256"] });
JS
printf 'db.getSiblingDB("$external").createUser({ user: "%s", roles: [{ role: "root", db: "admin" }] });\n' "$X509_USER" > x509.js

# The members of a replica set started on (port, dbpath, extra args).
rs_init_js() { # set name, ports...
  local name="$1"; shift
  local members="" i=0 prio
  for p in "$@"; do
    prio=$([ $i = 0 ] && echo 2 || echo 1)
    members="$members{ _id: $i, host: \"127.0.0.1:$p\", priority: $prio, tags: { dc: \"dc$i\" } },"
    i=$((i + 1))
  done
  printf 'rs.initiate({ _id: "%s", members: [%s] })' "$name" "$members"
}

wait_primary() { # shell command prefix…, port
  local port="${*: -1}"; set -- "${@:1:$#-1}"
  for _ in $(seq 1 120); do
    if "$@" --quiet --port "$port" --eval 'db.hello().isWritablePrimary' 2>/dev/null | grep -q true; then return 0; fi
    sleep 1
  done
  echo "no primary on $port" >&2; return 1
}

if [ -z "${MONGO_BIN:-}" ]; then
  # ---- Docker -----------------------------------------------------------------
  run() { docker run -d --name "$1" "${@:2}" >/dev/null; }
  sh_in() { local c="$1"; shift; docker exec -i "$c" "$@"; }
  if has standalone; then
    docker rm -f kernway-mongo >/dev/null 2>&1 || true
    # MONGO_SOCKET_DIR (Linux): the container's /tmp, where mongod puts its
    # socket, shared with the host.
    SOCKMOUNT=()
    if [ -n "${MONGO_SOCKET_DIR:-}" ]; then
      mkdir -p "$MONGO_SOCKET_DIR" && chmod 777 "$MONGO_SOCKET_DIR"
      SOCKMOUNT=(-v "$MONGO_SOCKET_DIR:/tmp")
    fi
    run kernway-mongo -p 27100:27017 ${SOCKMOUNT[@]+"${SOCKMOUNT[@]}"} "$IMAGE" mongod --auth --bind_ip_all --filePermissions 0777
    for _ in $(seq 1 60); do sh_in kernway-mongo mongosh --quiet --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done
    # The localhost exception creates the first user.
    sh_in kernway-mongo mongosh --quiet admin --eval 'db.createUser({user:"root",pwd:"root_pw",roles:["root"]})' >/dev/null
    sh_in kernway-mongo mongosh --quiet -u root -p root_pw < users.js >/dev/null
  fi
  if has tls; then
    docker rm -f kernway-mongo-tls >/dev/null 2>&1 || true
    docker create --name kernway-mongo-tls -p 27101:27017 "$IMAGE" mongod --bind_ip_all --auth --tlsMode requireTLS \
      --tlsCertificateKeyFile /certs/server.pem --tlsCAFile /certs/ca.pem --tlsAllowConnectionsWithoutCertificates >/dev/null
    docker cp "$DIR/." kernway-mongo-tls:/certs
    docker start kernway-mongo-tls >/dev/null
    TLS_SH=(mongosh --quiet --tls --tlsCAFile /certs/ca.pem --tlsCertificateKeyFile /certs/client.pem --host localhost)
    for _ in $(seq 1 60); do sh_in kernway-mongo-tls "${TLS_SH[@]}" --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done
    sh_in kernway-mongo-tls "${TLS_SH[@]}" admin --eval 'db.createUser({user:"root",pwd:"root_pw",roles:["root"]})' >/dev/null
    sh_in kernway-mongo-tls "${TLS_SH[@]}" -u root -p root_pw < x509.js >/dev/null
  fi
  if has rs; then
    docker rm -f kernway-mongo-rs >/dev/null 2>&1 || true
    run kernway-mongo-rs -p 27111:27111 -p 27112:27112 -p 27113:27113 "$IMAGE" bash -c '
      for p in 27111 27112 27113; do mkdir -p /data/rs$p; mongod --replSet rs0 --port $p --dbpath /data/rs$p --bind_ip_all --setParameter enableTestCommands=1 --fork --logpath /data/rs$p.log; done
      exec tail -f /dev/null'
    for p in 27111 27112 27113; do
      for _ in $(seq 1 60); do sh_in kernway-mongo-rs mongosh --quiet --port $p --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done
    done
    sh_in kernway-mongo-rs mongosh --quiet --port 27111 --eval "$(rs_init_js rs0 27111 27112 27113)" >/dev/null
    wait_primary docker exec kernway-mongo-rs mongosh 27111
  fi
  if has sharded; then
    docker rm -f kernway-mongo-sh >/dev/null 2>&1 || true
    run kernway-mongo-sh -p 27120:27120 -p 27121:27121 -p 27122:27122 -p 27123:27123 "$IMAGE" bash -c '
      mkdir -p /data/cfg /data/sh0
      mongod --configsvr --replSet cfg --port 27121 --dbpath /data/cfg --bind_ip_all --setParameter enableTestCommands=1 --fork --logpath /data/cfg.log
      mongod --shardsvr --replSet sh0 --port 27122 --dbpath /data/sh0 --bind_ip_all --setParameter enableTestCommands=1 --fork --logpath /data/sh0.log
      exec tail -f /dev/null'
    for p in 27121 27122; do
      for _ in $(seq 1 60); do sh_in kernway-mongo-sh mongosh --quiet --port $p --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done
    done
    sh_in kernway-mongo-sh mongosh --quiet --port 27121 --eval "$(rs_init_js cfg 27121)" >/dev/null
    sh_in kernway-mongo-sh mongosh --quiet --port 27122 --eval "$(rs_init_js sh0 27122)" >/dev/null
    wait_primary docker exec kernway-mongo-sh mongosh 27121
    wait_primary docker exec kernway-mongo-sh mongosh 27122
    for p in 27120 27123; do
      docker exec kernway-mongo-sh mongos --configdb cfg/127.0.0.1:27121 --port $p --bind_ip_all --setParameter enableTestCommands=1 --fork --logpath /data/mongos$p.log >/dev/null
    done
    for _ in $(seq 1 60); do sh_in kernway-mongo-sh mongosh --quiet --port 27120 --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done
    sh_in kernway-mongo-sh mongosh --quiet --port 27120 --eval 'sh.addShard("sh0/127.0.0.1:27122")' >/dev/null
  fi
  VERSION="$(docker run --rm "$IMAGE" mongod --version | head -1)"
else
  # ---- native -----------------------------------------------------------------
  MONGOSH="${MONGOSH:-$MONGO_BIN/mongosh$EXE}"
  start() { # name, args...
    local name="$1"; shift
    mkdir -p "$DIR/$name"
    "$MONGO_BIN/mongod$EXE" --dbpath "$(native_path "$DIR/$name")" --logpath "$(native_path "$DIR/$name.log")" "$@" > "$DIR/$name.out" 2>&1 &
  }
  wait_up() { for _ in $(seq 1 90); do "$MONGOSH" --quiet "$@" --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && return 0; sleep 1; done; echo "mongod $* did not start" >&2; exit 1; }
  if has standalone; then
    SOCK=()
    [ -z "$EXE" ] && SOCK=(--unixSocketPrefix /tmp)
    start standalone --port 27100 --bind_ip 127.0.0.1 --auth ${SOCK[@]+"${SOCK[@]}"}
    wait_up --port 27100
    "$MONGOSH" --quiet --port 27100 admin --eval 'db.createUser({user:"root",pwd:"root_pw",roles:["root"]})' >/dev/null
    "$MONGOSH" --quiet --port 27100 -u root -p root_pw < users.js >/dev/null
  fi
  if has tls; then
    start tls --port 27101 --bind_ip localhost --auth --tlsMode requireTLS --tlsCertificateKeyFile "$(native_path "$DIR/server.pem")" \
      --tlsCAFile "$(native_path "$DIR/ca.pem")" --tlsAllowConnectionsWithoutCertificates
    TLS_SH=(--tls --tlsCAFile "$(native_path "$DIR/ca.pem")" --tlsCertificateKeyFile "$(native_path "$DIR/client.pem")" --host localhost --port 27101)
    wait_up "${TLS_SH[@]}"
    "$MONGOSH" --quiet "${TLS_SH[@]}" admin --eval 'db.createUser({user:"root",pwd:"root_pw",roles:["root"]})' >/dev/null
    "$MONGOSH" --quiet "${TLS_SH[@]}" -u root -p root_pw < x509.js >/dev/null
  fi
  if has rs; then
    for p in 27111 27112 27113; do start "rs$p" --replSet rs0 --port $p --bind_ip 127.0.0.1 --setParameter enableTestCommands=1; done
    for p in 27111 27112 27113; do wait_up --port $p; done
    "$MONGOSH" --quiet --port 27111 --eval "$(rs_init_js rs0 27111 27112 27113)" >/dev/null
    wait_primary "$MONGOSH" 27111
  fi
  if has sharded; then
    start cfg --configsvr --replSet cfg --port 27121 --bind_ip 127.0.0.1 --setParameter enableTestCommands=1
    start sh0 --shardsvr --replSet sh0 --port 27122 --bind_ip 127.0.0.1 --setParameter enableTestCommands=1
    wait_up --port 27121
    wait_up --port 27122
    "$MONGOSH" --quiet --port 27121 --eval "$(rs_init_js cfg 27121)" >/dev/null
    "$MONGOSH" --quiet --port 27122 --eval "$(rs_init_js sh0 27122)" >/dev/null
    wait_primary "$MONGOSH" 27121
    wait_primary "$MONGOSH" 27122
    for p in 27120 27123; do
      "$MONGO_BIN/mongos$EXE" --configdb cfg/127.0.0.1:27121 --port $p --bind_ip 127.0.0.1 --setParameter enableTestCommands=1 --logpath "$(native_path "$DIR/mongos$p.log")" > "$DIR/mongos$p.out" 2>&1 &
    done
    wait_up --port 27120
    "$MONGOSH" --quiet --port 27120 --eval 'sh.addShard("sh0/127.0.0.1:27122")' >/dev/null
  fi
  VERSION="$("$MONGO_BIN/mongod$EXE" --version | head -1)"
fi

echo "$VERSION" >&2
has standalone && echo "KERNWAY_MONGO_URL=mongodb://kernway:kernway_pw@127.0.0.1:27100/kernway_test?authSource=admin"
has tls && echo "KERNWAY_MONGO_TLS_DIR=$(native_path "$DIR")"
has rs && echo "KERNWAY_MONGO_RS_URL=mongodb://127.0.0.1:27111,127.0.0.1:27112/kernway_test?replicaSet=rs0"
has sharded && echo "KERNWAY_MONGO_SHARDED_URL=mongodb://127.0.0.1:27120,127.0.0.1:27123/kernway_test"
if [ -n "${MONGO_BIN:-}" ] && [ -z "$EXE" ] && has standalone; then echo "KERNWAY_MONGO_SOCKET=/tmp/mongodb-27100.sock"; fi
if [ -z "${MONGO_BIN:-}" ] && [ -n "${MONGO_SOCKET_DIR:-}" ] && has standalone; then echo "KERNWAY_MONGO_SOCKET=$MONGO_SOCKET_DIR/mongodb-27017.sock"; fi
exit 0
