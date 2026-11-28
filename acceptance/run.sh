#!/usr/bin/env bash
# Acceptance run for one database driver: bring up (or reuse) the database in
# Docker, prepare accounts / certificates, run every test of the driver one at
# a time against it, and write docs/acceptance/<db>.md.
#
#   acceptance/run.sh mysql
#   acceptance/run.sh oracle
#   acceptance/run.sh sqlite   (embedded: no container, real database files)
#   acceptance/run.sh redis    (single node, TLS, Sentinel, Cluster — all in Docker)
#   acceptance/run.sh postgres (PostgreSQL 18 with every auth method and TLS, in Docker)
#   acceptance/run.sh mongodb  (MongoDB 8.0: standalone, TLS / X.509, replica set, sharded — in Docker)
#   acceptance/run.sh mssql    (SQL Server 2022 with a test CA, in Docker)
#   acceptance/run.sh s3       (the Versity S3 Gateway, natively — S3-compatible object storage)
#   acceptance/run.sh mariadb  (MariaDB 11.8 LTS through the MySQL driver, in Docker)
#   acceptance/run.sh valkey   (Valkey 8.1 through the Redis client: single, TLS, Sentinel, Cluster — in Docker)
#   acceptance/run.sh nats     (nats-server natively: plain, auth, TLS / mTLS, a JetStream cluster)
#   acceptance/run.sh rabbitmq (RabbitMQ 4.1 with TLS and EXTERNAL auth, in Docker)
#   acceptance/run.sh meilisearch (the official Meilisearch binary, natively)
#   acceptance/run.sh kafka    (Kafka 3.9 KRaft with SASL PLAIN / SCRAM and TLS, in Docker)
#   acceptance/run.sh http-client (go-httpbin + Caddy, native; HTTPBIN_DOCKER=1 for Docker)
#   acceptance/run.sh ssh      (OpenSSH in Docker; SSH_NATIVE=1 for sshd as this user, key logins only)
#   acceptance/run.sh security (attacks on a live Kernway server; session / ban stores on Redis in Docker)
#
# Containers (created if missing; existing ones and their data are reused):
#   kernway-mysql   mysql:8.0                  host port 3307, root password "secret"
#   kernway-oracle  gvenzl/oracle-free:23-slim host ports 1521 / 2484 (TCPS)
# Oracle TCPS needs a wallet once: crates/kernway-orm-oracle/tests/support/oracle-tcps-setup.sh
set -euo pipefail
DB="${1:?usage: $0 mysql|oracle|sqlite|redis|postgres|mongodb|mssql|nats|rabbitmq|meilisearch|kafka|mariadb|valkey|s3|http-client|ssh|security}"
# CI workflow whose jobs for this commit are the cross-OS evidence.
WORKFLOW=drivers.yml; IMAGE=""; EXTRA=()
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/docs/acceptance"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

log() { printf '\033[1m==> %s\033[0m\n' "$*" >&2; }

running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || echo missing)" = "true" ]; }

ensure_container() { # name, run args...
  local name="$1"; shift
  if running "$name"; then return; fi
  if docker inspect "$name" >/dev/null 2>&1; then
    log "starting existing container $name"; docker start "$name" >/dev/null
  else
    log "creating container $name"; docker run -d --name "$name" "$@" >/dev/null
  fi
}

wait_for() { # description, command...
  local what="$1"; shift
  for _ in $(seq 1 120); do
    if "$@" >/dev/null 2>&1; then return; fi
    sleep 5
  done
  echo "timed out waiting for $what" >&2; exit 2
}

case "$DB" in
  mysql)
    C="${MYSQL_CONTAINER:-kernway-mysql}"; PORT="${MYSQL_PORT:-3307}"; RPW="${MYSQL_ROOT_PASSWORD:-secret}"
    ensure_container "$C" -p "$PORT:3306" -e MYSQL_ROOT_PASSWORD="$RPW" mysql:8.0
    log "waiting for MySQL"
    wait_for "MySQL" docker exec "$C" mysqladmin -uroot -p"$RPW" ping --silent
    log "preparing accounts"
    docker exec -i "$C" mysql -uroot -p"$RPW" 2>/dev/null <<SQL
CREATE DATABASE IF NOT EXISTS kernway_test;
CREATE USER IF NOT EXISTS 'kernway'@'%' IDENTIFIED WITH mysql_native_password BY 'kernway_pw';
GRANT ALL ON kernway_test.* TO 'kernway'@'%';
CREATE USER IF NOT EXISTS 'kw_sha2'@'%' IDENTIFIED WITH caching_sha2_password BY 'sha2_pw';
GRANT ALL ON kernway_test.* TO 'kw_sha2'@'%';
SQL
    mkdir -p "$WORK/certs"
    for f in ca.pem client-cert.pem client-key.pem public_key.pem; do
      docker cp "$C:/var/lib/mysql/$f" "$WORK/certs/$f"
    done
    export KERNWAY_MYSQL_URL="mysql://kernway:kernway_pw@127.0.0.1:$PORT/kernway_test"
    export KERNWAY_MYSQL_ROOT_URL="mysql://root:$RPW@127.0.0.1:$PORT/kernway_test"
    export KERNWAY_MYSQL_SHA2_URL="mysql://kw_sha2:sha2_pw@127.0.0.1:$PORT/kernway_test"
    export KERNWAY_MYSQL_CA="$WORK/certs/ca.pem"
    export KERNWAY_MYSQL_CERTS="$WORK/certs"
    VERSION="MySQL $(docker exec "$C" mysql -uroot -p"$RPW" -N -e 'SELECT VERSION()' 2>/dev/null)"
    CRATE=kernway-orm-mysql; FEATURES=(--features mysql); TITLE=MySQL
    # Local-channel tests need the server's socket file: they run inside the
    # MySQL container (see in_container_tests), not on the host.
    SKIP=(--skip local_socket_ --skip mariadb_)
    ;;
  oracle)
    C="${ORACLE_CONTAINER:-kernway-oracle}"
    ensure_container "$C" -p 1521:1521 -p 2484:2484 -e ORACLE_PASSWORD=oracle_pw \
      -e APP_USER=kernway -e APP_USER_PASSWORD=kernway_pw gvenzl/oracle-free:23-slim
    log "waiting for Oracle (first start takes a few minutes)"
    wait_for "Oracle" docker exec "$C" /opt/oracle/healthcheck.sh
    export KERNWAY_ORACLE_HOST=127.0.0.1
    if docker cp "$C:/opt/oracle/wallet/server.crt" "$WORK/server.crt" 2>/dev/null; then
      export KERNWAY_ORACLE_CA="$WORK/server.crt"
      # A server set up with --mtls: its client certificate.
      if docker cp "$C:/opt/oracle/wallet/client" "$WORK/client" 2>/dev/null; then
        export KERNWAY_ORACLE_CLIENT_CERT="$WORK/client/client.crt" KERNWAY_ORACLE_CLIENT_KEY="$WORK/client/client.key"
        export KERNWAY_ORACLE_CLIENT_PEM="$WORK/client/client.pem"
      fi
    else
      log "no TCPS wallet in $C: the TCPS test will be reported as skipped (run oracle-tcps-setup.sh)"
    fi
    VERSION="$(docker exec "$C" bash -c "echo 'SET HEADING OFF
SET LINESIZE 400
SELECT banner_full FROM v\$version;' | sqlplus -s system/oracle_pw@//localhost:1521/FREEPDB1" | tr -s ' \n' ' ' | sed 's/^ //; s/ $//')"
    CRATE=kernway-orm-oracle; FEATURES=(); TITLE=Oracle; SKIP=()
    ;;
  sqlite)
    # Embedded: the engine is compiled into the driver, the tests open real
    # database files in the temp directory. Nothing to start.
    C=""; IMAGE="vendored in the driver (crates/kernway-orm-sqlite/sqlite, official amalgamation), own C-API binding"; VERSION=""
    CRATE=kernway-orm-sqlite; FEATURES=(); TITLE=SQLite; SKIP=()
    # The workspace CI runs every crate's tests on Linux, macOS and Windows.
    WORKFLOW=workspace.yml
    ;;
  redis|valkey)
    # Four deployments: a single node (with a Unix socket), TLS-only (with
    # client certificates), Sentinel (master + replica + 3 sentinels) and a
    # 3-master / 3-replica Cluster. The TLS, Sentinel and Cluster containers
    # are recreated each run (test data only). Valkey: the same, on its image
    # (it keeps redis-server / redis-cli as names).
    if [ "$DB" = valkey ]; then
      export REDIS_IMAGE="${VALKEY_IMAGE:-valkey/valkey:8.1-alpine}"
      C="${VALKEY_CONTAINER:-kernway-valkey}"; IMAGE_TAG="$REDIS_IMAGE"
    else
      C="${REDIS_CONTAINER:-kernway-redis}"; IMAGE_TAG="${REDIS_IMAGE:-redis:7.4-alpine}"
    fi
    S="$ROOT/crates/kernway-redis/tests/support"
    ensure_container "$C" -p 6380:6379 "$IMAGE_TAG" redis-server --unixsocket /tmp/redis.sock --unixsocketperm 777
    wait_for "Redis" docker exec "$C" redis-cli PING
    # The ACL user the pub/sub and cache tests authenticate as.
    docker exec "$C" redis-cli ACL SETUSER kernway on '>kernway_pw' '~*' '&*' '+@all' >/dev/null
    export KERNWAY_REDIS_USER=kernway KERNWAY_REDIS_PASS=kernway_pw
    log "TLS, Sentinel and Cluster deployments"
    "$S/redis-tls-setup.sh" "$WORK/redis-tls" >/dev/null
    "$S/redis-sentinel-setup.sh" >/dev/null
    "$S/redis-cluster-setup.sh" >/dev/null
    export KERNWAY_REDIS_URL=redis://127.0.0.1:6380 KW_REDIS_ADDR=127.0.0.1:6380 KERNWAY_REDIS_ADDR=127.0.0.1:6380
    export KERNWAY_REDIS_TLS_URL=rediss://localhost:6381 KERNWAY_REDIS_TLS_DIR="$WORK/redis-tls"
    export KERNWAY_REDIS_SENTINEL_URL=redis+sentinel://127.0.0.1:26390,127.0.0.1:26391,127.0.0.1:26392/kwmaster
    export KERNWAY_REDIS_CLUSTER_URL=redis+cluster://127.0.0.1:7000,127.0.0.1:7001
    VERSION="$(docker exec "$C" redis-server --version | sed 's/ sha=.*//')"
    CRATE=kernway-redis; FEATURES=(); TITLE=Redis
    if [ "$DB" = valkey ]; then TITLE="Valkey (Redis client)"; fi
    # The tests marked #[ignore] need a server: run them. The Unix-socket
    # test runs inside the Redis container (in_container_tests).
    EXTRA=(--include-ignored); SKIP=(--skip unix_socket_)
    ;;
  postgres)
    # Recreated each run (test data only): roles for SCRAM / MD5 / clear-text
    # / trust / certificate authentication, TLS with a test CA.
    C="${PG_CONTAINER:-kernway-postgres}"
    log "PostgreSQL (${PG_IMAGE:-postgres:18-alpine}) with TLS and every auth method"
    PG_CONTAINER="$C" "$ROOT/crates/kernway-orm-postgres/tests/support/pg-setup.sh" "$WORK/pg-tls" > "$WORK/pg.env"
    set -a; . "$WORK/pg.env"; set +a
    VERSION="$(docker exec "$C" postgres --version)"
    CRATE=kernway-orm-postgres; FEATURES=(--features postgres); TITLE=PostgreSQL
    # The Unix-socket test runs inside the container (in_container_tests).
    SKIP=(--skip unix_domain_socket)
    ;;
  mongodb)
    # Recreated each run (test data only): a standalone with SCRAM users, a
    # TLS-only server with an X.509 user, a 3-member replica set and a
    # sharded cluster with two routers.
    C=kernway-mongo
    log "MongoDB (${MONGO_IMAGE:-mongo:8.0}): standalone, TLS, replica set, sharded cluster"
    "$ROOT/crates/kernway-orm-mongodb/tests/support/mongo-setup.sh" "$WORK/mongo" > "$WORK/mongo.env"
    set -a; . "$WORK/mongo.env"; set +a
    # The official specifications' test data (BSON corpus, URI tests…), pinned.
    "$ROOT/crates/kernway-orm-mongodb/tests/support/specs-fetch.sh" "$WORK/mongo-specs" > "$WORK/mongo-specs.env"
    set -a; . "$WORK/mongo-specs.env"; set +a
    VERSION="MongoDB $(docker exec "$C" mongod --version | sed -n 1p | sed 's/db version v//')"
    CRATE=kernway-orm-mongodb; FEATURES=(); TITLE=MongoDB
    # The Unix-socket test runs inside the container (in_container_tests).
    SKIP=(--skip unix_domain_socket)
    ;;
  mssql)
    # Recreated each run (test data only): SQL Server 2022 with a server
    # certificate from a test CA, a login owning kernway_test.
    C=kernway-mssql
    log "SQL Server (${MSSQL_IMAGE:-mcr.microsoft.com/mssql/server:2022-latest}) with TLS"
    "$ROOT/crates/kernway-orm-mssql/tests/support/mssql-setup.sh" "$WORK/mssql" > "$WORK/mssql.env"
    set -a; . "$WORK/mssql.env"; set +a
    VERSION="$(docker exec "$C" /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P 'Kernway_pw1!' -h -1 -Q 'SET NOCOUNT ON; SELECT @@VERSION' | sed -n 1p | sed 's/ *$//')"
    CRATE=kernway-orm-mssql; FEATURES=(--features mssql); TITLE="SQL Server"; SKIP=()
    ;;
  nats)
    # Native nats-server processes (the official release binary, as in CI):
    # plain JetStream, auth (user / NKey), token, TLS, TLS-first, mutual TLS
    # and a three-node JetStream cluster. Recreated each run.
    log "nats-server (native): plain, auth, token, TLS, mTLS, 3-node JetStream cluster"
    "$ROOT/crates/kernway-nats/tests/support/nats-setup.sh" "$ROOT/target/nats" > "$WORK/nats.env"
    set -a; . "$WORK/nats.env"; set +a
    VERSION="nats-server $KERNWAY_NATS_VERSION"; IMAGE="official release binary (native)"; C="none (native nats-server processes)"
    CRATE=kernway-nats; FEATURES=(); TITLE="NATS / JetStream"; SKIP=()
    ;;
  rabbitmq)
    # Recreated each run (test data only): RabbitMQ with a test CA, TLS,
    # EXTERNAL (client-certificate) auth, users and a second vhost.
    C=kernway-rabbitmq
    log "RabbitMQ (${RABBITMQ_IMAGE:-rabbitmq:4.1-management}) with TLS and EXTERNAL auth"
    "$ROOT/crates/kernway-rabbitmq/tests/support/rabbitmq-setup.sh" "$ROOT/target/rabbitmq" > "$WORK/rabbitmq.env"
    # The rabbitmqctl command holds spaces: quote it for sourcing.
    sed -i.bak 's/^\(KERNWAY_RABBITMQ_[A-Z]*=\)\(.*\)$/\1"\2"/' "$WORK/rabbitmq.env"
    set -a; . "$WORK/rabbitmq.env"; set +a
    VERSION="RabbitMQ $KERNWAY_RABBITMQ_VERSION"
    CRATE=kernway-rabbitmq; FEATURES=(); TITLE=RabbitMQ; SKIP=()
    ;;
  meilisearch)
    # The official release binary, natively, with a fresh data dir.
    log "Meilisearch (native release binary)"
    "$ROOT/crates/kernway-orm-meilisearch/tests/support/meili-setup.sh" "$ROOT/target/meili" > "$WORK/meili.env"
    set -a; . "$WORK/meili.env"; set +a
    VERSION="Meilisearch $MEILI_VERSION"; IMAGE="official release binary (native)"; C="none (native meilisearch process)"
    CRATE=kernway-orm-meilisearch; FEATURES=(--features meilisearch); TITLE=Meilisearch; SKIP=()
    # Its live tests are #[ignore]d for plain `cargo test`: run them here.
    EXTRA=(--include-ignored)
    ;;
  kafka)
    # Recreated each run (test data only): one KRaft node with PLAINTEXT,
    # SASL (PLAIN, SCRAM-SHA-256/512) and TLS listeners.
    C=kernway-kafka
    log "Kafka (${KAFKA_IMAGE:-apache/kafka:3.9.1}): KRaft, SASL, TLS"
    "$ROOT/crates/kernway-kafka/tests/support/kafka-setup.sh" "$ROOT/target/kafka" > "$WORK/kafka.env"
    sed -i.bak 's/^\(KERNWAY_KAFKA_[A-Z_]*=\)\(.*\)$/\1"\2"/' "$WORK/kafka.env"
    set -a; . "$WORK/kafka.env"; set +a
    VERSION="Kafka $KERNWAY_KAFKA_VERSION (KRaft)"
    CRATE=kernway-kafka; FEATURES=(); TITLE=Kafka; SKIP=()
    ;;
  mariadb)
    # Recreated each run (test data only): MariaDB with a test CA, the MySQL
    # driver's accounts and database.
    C=kernway-mariadb
    log "MariaDB (${MARIADB_IMAGE:-mariadb:11.8}) through the MySQL driver"
    "$ROOT/crates/kernway-orm-mysql/tests/support/mariadb-setup.sh" "$ROOT/target/mariadb" > "$WORK/mariadb.env"
    set -a; . "$WORK/mariadb.env"; set +a
    VERSION="MariaDB $KERNWAY_MARIADB_VERSION"
    CRATE=kernway-orm-mysql; FEATURES=(--features mysql); TITLE="MariaDB (MySQL driver)"
    # caching_sha2_password is MySQL's; the local channel runs in the container.
    SKIP=(--skip local_socket_ --skip caching_sha2)
    ;;
  s3)
    # The Versity S3 Gateway (natively) over a fresh posix directory.
    log "S3-compatible storage (versitygw, native)"
    "$ROOT/crates/kernway-s3/tests/support/s3-setup.sh" "$ROOT/target/s3" > "$WORK/s3.env"
    # KERNWAY_S3_SERVER holds a space ("versitygw 1.11.0"): quote for sourcing.
    sed -i.bak 's/^\(KERNWAY_S3_[A-Z_]*=\)\(.*\)$/\1"\2"/' "$WORK/s3.env"
    set -a; . "$WORK/s3.env"; set +a
    VERSION="$KERNWAY_S3_SERVER"; IMAGE="official release binary (native)"; C="none (native versitygw process)"
    CRATE=kernway-s3; FEATURES=(); TITLE="S3-compatible object storage"; SKIP=()
    ;;
  http-client)
    # go-httpbin (plain + TLS) and Caddy (mTLS, precompressed brotli), with a fresh test CA.
    log "HTTP servers (go-httpbin, Caddy)"
    "$ROOT/crates/kernway-http-client/tests/support/httpbin-setup.sh" "$ROOT/target/httpbin" > "$WORK/httpbin.env"
    sed -i.bak 's/^\(KERNWAY_HTTPBIN_[A-Z_]*=\)\(.*\)$/\1"\2"/' "$WORK/httpbin.env"
    set -a; . "$WORK/httpbin.env"; set +a
    VERSION="$KERNWAY_HTTPBIN_SERVER"
    if [ -n "${HTTPBIN_DOCKER:-}" ]; then
      IMAGE="ghcr.io/mccutchen/go-httpbin, caddy"; C="servers in Docker: kernway-httpbin, kernway-httpbin-tls, kernway-caddy"
    else
      IMAGE="official release binaries (native)"; C="none (native go-httpbin and Caddy processes)"
    fi
    CRATE=kernway-http-client; FEATURES=(); TITLE="HTTP client"; SKIP=()
    ;;
  ssh)
    # OpenSSH with fresh keys: in Docker (password login too), or natively as this user.
    log "OpenSSH server"
    if [ -z "${SSH_NATIVE:-}" ]; then export SSH_DOCKER=1; fi
    "$ROOT/crates/kernway-ssh/tests/support/ssh-setup.sh" "$ROOT/target/ssh" > "$WORK/ssh.env"
    sed -i.bak 's/^\(KERNWAY_SSH_[A-Z_]*=\)\(.*\)$/\1"\2"/' "$WORK/ssh.env"
    set -a; . "$WORK/ssh.env"; set +a
    VERSION="$KERNWAY_SSH_SERVER"; CRATE=kernway-ssh; FEATURES=(); TITLE="SSH and SFTP client"; SKIP=()
    if [ -n "${SSH_DOCKER:-}" ]; then
      IMAGE="lscr.io/linuxserver/openssh-server"; C="servers in Docker: kernway-sshd"
    else
      IMAGE="the system's sshd (native)"; C="none (native sshd processes)"; SKIP=(--skip password_)
    fi
    ;;
  security)
    # The attack tests start their own Kernway servers; the Redis-backed session,
    # presence and ban stores get a Redis of their own.
    C=kernway-security-redis; IMAGE="${SECURITY_REDIS_IMAGE:-redis:7.4-alpine}"
    log "Redis for the session / ban stores ($IMAGE)"
    ensure_container "$C" -p 6382:6379 "$IMAGE"
    wait_for "Redis" docker exec "$C" redis-cli PING
    export KW_REDIS_ADDR=127.0.0.1:6382
    VERSION="Kernway server (this commit) + $(docker exec "$C" redis-server --version | cut -d' ' -f3)"
    CRATE=kernway-server; TITLE="Web security"; SKIP=()
    FEATURES=(-p kernway-security -p kernway-http --all-features)
    # The Redis store tests are #[ignore]d for plain `cargo test`: run them here.
    EXTRA=(--include-ignored)
    ;;
  *) echo "unknown database: $DB (mysql | oracle | sqlite | redis | postgres | mongodb | mssql | nats | rabbitmq | meilisearch | kafka | mariadb | valkey | s3 | http-client | ssh | security)" >&2; exit 2 ;;
esac

CMD="cargo test -p $CRATE ${FEATURES[*]+${FEATURES[*]}} --lib --tests -- --test-threads=1 --nocapture ${EXTRA[*]+${EXTRA[*]}} ${SKIP[*]+${SKIP[*]}}"
log "running: $CMD"
( cd "$ROOT" && cargo test -p "$CRATE" ${FEATURES[@]+"${FEATURES[@]}"} --lib --tests -- --test-threads=1 --nocapture ${EXTRA[@]+"${EXTRA[@]}"} ${SKIP[@]+"${SKIP[@]}"} ) > "$WORK/test.log" 2>&1 || true
if ! grep -q "^test result:" "$WORK/test.log"; then
  log "cargo test did not run any test binary:"; tail -20 "$WORK/test.log" >&2
fi

if [ "$DB" = sqlite ]; then
  VERSION="SQLite $(sed -n 's/.*SQLite engine: //p' "$WORK/test.log" | sed -n 1p)"
fi

# Tests that must run next to the server (its Unix socket): build a static
# Linux test binary in a Rust container (cached volumes), copy it into the
# database container and run it there. Their results join the same log; if
# the build fails they are reported as not run.
in_container_tests() { # test target, name filter, env...
  local target="$1" filter="$2"; shift 2
  log "building $target for Linux (rust:1-alpine) to run inside $C"
  local exe
  exe="$(docker run --rm -v "$ROOT":/src:ro -v kernway-acc-cargo:/usr/local/cargo/registry \
      -v kernway-acc-target:/target -e CARGO_TARGET_DIR=/target -w /src rust:1-alpine sh -c \
      "apk add --no-cache musl-dev >/dev/null && cargo test --locked -p $CRATE ${FEATURES[*]+${FEATURES[*]}} --test $target --no-run --message-format short 2>&1" \
      | sed -n 's/.*Executable .*(\(.*\)).*/\1/p' | tail -1)"
  if [ -z "$exe" ]; then log "in-container build failed: $target tests not run"; return; fi
  docker run --rm -v kernway-acc-target:/target rust:1-alpine cat "$exe" > "$WORK/$target-linux"
  chmod +x "$WORK/$target-linux"
  docker cp "$WORK/$target-linux" "$C:/tmp/kernway-$target"
  local envs=(); for e in "$@"; do envs+=(-e "$e"); done
  log "running $target $filter inside $C"
  docker exec "${envs[@]}" "$C" "/tmp/kernway-$target" "$filter" --test-threads=1 --nocapture >> "$WORK/test.log" 2>&1 || true
  docker exec "$C" rm -f "/tmp/kernway-$target"
}
if [ "$DB" = redis ] || [ "$DB" = valkey ]; then
  in_container_tests connection unix_socket_ KERNWAY_REDIS_SOCKET=/tmp/redis.sock
  CMD="$CMD; unix_socket_* inside $C"
fi
if [ "$DB" = mongodb ]; then
  in_container_tests connection unix_domain_socket KERNWAY_MONGO_SOCKET=/tmp/mongodb-27017.sock
  CMD="$CMD; unix_domain_socket inside $C"
fi
if [ "$DB" = postgres ]; then
  in_container_tests postgres unix_domain_socket KERNWAY_PG_SOCKET_DIR=/var/run/postgresql KERNWAY_PG_SOCKET_PORT=5432
  CMD="$CMD; unix_domain_socket inside $C"
fi
if [ "$DB" = mysql ]; then
  in_container_tests connection local_socket_ \
    KERNWAY_MYSQL_SOCKET=/var/run/mysqld/mysqld.sock KERNWAY_MYSQL_ROOT_PASSWORD="$RPW"
  CMD="$CMD; local_socket_* inside $C"
fi
if [ "$DB" = mariadb ]; then
  # Not local_socket_carries_a_caching_sha2…: caching_sha2_password is MySQL's.
  for t in local_socket_connects local_socket_refuses; do
    in_container_tests connection "$t" \
      KERNWAY_MYSQL_SOCKET=/run/mysqld/mysqld.sock KERNWAY_MYSQL_ROOT_PASSWORD=secret
  done
  CMD="$CMD; local_socket_connects* / local_socket_refuses* inside $C"
fi

# Every-OS evidence: the CI workflow ($WORKFLOW) runs the driver's tests on
# Linux, macOS and Windows. Its jobs for exactly this commit
# join the log as tests `ci::<job>`; no run for this commit = not run = FAIL.
ci_evidence() {
  local sha run
  sha="$(cd "$ROOT" && git rev-parse HEAD)"
  run="$(cd "$ROOT" && gh run list --workflow "$WORKFLOW" --commit "$sha" --status completed --limit 1 \
      --json databaseId -q '.[0].databaseId' 2>/dev/null || true)"
  if [ -z "$run" ]; then log "no completed $WORKFLOW CI run for $sha: cross-OS evidence missing"; return; fi
  log "cross-OS evidence: CI run $run for $sha"
  (cd "$ROOT" && gh run view "$run" --json jobs \
      -q '.jobs[] | "test ci::" + (.name | gsub("[ ()-]+"; "_") | sub("_$"; "")) + " ... " + (if .conclusion == "success" then "ok" else "FAILED" end)') \
    >> "$WORK/test.log" 2>/dev/null || true
  CMD="$CMD; CI run $run (Linux / macOS / Windows)"
}
ci_evidence

python3 - "$WORK/meta.json" <<PY
import json, subprocess, sys, datetime
def sh(*a):
    try: return subprocess.check_output(a, cwd="$ROOT", text=True, stderr=subprocess.DEVNULL).strip()
    except Exception: return "?"
dirty = " (uncommitted changes)" if sh("git", "status", "--porcelain", "--", ".", ":!docs/acceptance") else ""
json.dump({
  "generated": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
  "server": """$VERSION""",
  "image": """$IMAGE""" or sh("docker", "inspect", "-f", "{{.Config.Image}}", "$C"),
  "container": "$C" or "none (embedded)",
  "driver commit": sh("git", "rev-parse", "--short", "HEAD") + " on " + sh("git", "rev-parse", "--abbrev-ref", "HEAD") + dirty,
  "rustc": sh("rustc", "--version"),
  "command": "\`$CMD\`",
}, open(sys.argv[1], "w"))
PY

set +e
python3 "$ROOT/acceptance/report.py" "$TITLE" "$ROOT/acceptance/$DB.features" "$WORK/test.log" "$WORK/meta.json" > "$OUT/$DB.md"
STATUS=$?
set -e
# The cross-driver list of gaps follows the catalogs.
python3 "$ROOT/acceptance/gaps.py"
grep -m3 -E '^## Verdict|^- Features|^- Tests' "$OUT/$DB.md" >&2
log "report: docs/acceptance/$DB.md"
exit $STATUS
