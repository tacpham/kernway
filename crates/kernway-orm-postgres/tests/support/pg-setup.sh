#!/usr/bin/env bash
# A PostgreSQL for the driver's tests, with every authentication method the
# driver speaks and TLS:
#
#   kernway / kernway_pw   SCRAM-SHA-256, owns database kernway_test
#   kw_md5 / md5_pw        MD5
#   kw_clear / clear_pw    clear-text password
#   kw_trust               trust (no password)
#   kw_cert                client certificate (hostssl, cert auth)
#   kw_ssl / ssl_pw        SCRAM, TLS connections only
#
# TLS uses a fresh CA and a server certificate for DNS:localhost only (so
# 127.0.0.1 exercises verify-full's name check); client.crt / client.key are
# for kw_cert. Local (Unix-socket) connections are trusted.
#
# Two ways to run the server:
#   Docker (default):    PG_IMAGE (postgres:18-alpine), PG_CONTAINER, and
#                        PG_SOCKET_DIR to share the socket directory (Linux)
#   native:              PG_BIN=<dir with initdb, pg_ctl, psql> — the OS's
#                        own PostgreSQL build (macOS / Windows CI runners)
#
#   crates/kernway-orm-postgres/tests/support/pg-setup.sh [dir]
#
# Prints the test environment (KERNWAY_PG_URL, KERNWAY_PG_TLS_DIR,
# KERNWAY_PG_SOCKET_DIR) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/pg-tls}"
NAME="${PG_CONTAINER:-kernway-postgres}"
PORT="${PG_PORT:-5433}"
IMAGE="${PG_IMAGE:-postgres:18-alpine}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"

# ---- certificates --------------------------------------------------------
export MSYS_NO_PATHCONV=1 # Git Bash: keep "/CN=…" as is
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw_cert" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null
chmod 600 client.key
unset MSYS_NO_PATHCONV

# ---- roles and access rules ----------------------------------------------
cat > roles.sql <<'SQL'
CREATE ROLE kernway LOGIN SUPERUSER PASSWORD 'kernway_pw';
CREATE DATABASE kernway_test OWNER kernway;
SET password_encryption = 'md5';
CREATE ROLE kw_md5 LOGIN PASSWORD 'md5_pw';
RESET password_encryption;
CREATE ROLE kw_clear LOGIN PASSWORD 'clear_pw';
CREATE ROLE kw_trust LOGIN;
CREATE ROLE kw_cert LOGIN;
CREATE ROLE kw_ssl LOGIN PASSWORD 'ssl_pw';
GRANT ALL ON DATABASE kernway_test TO kw_md5, kw_clear, kw_trust, kw_cert, kw_ssl;
SQL
cat > pg_hba.conf <<'HBA'
local     all all                trust
hostssl   all kw_cert    all     cert
host      all kw_trust   all     trust
host      all kw_md5     all     md5
host      all kw_clear   all     password
hostnossl all kw_ssl     all     reject
host      all all        all     scram-sha-256
HBA

if [ -n "${PG_BIN:-}" ]; then
  # ---- native server -----------------------------------------------------
  EXE=""
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
  DATA="$DIR/data"
  rm -rf "$DATA"
  # Native Windows programs take C:/… paths.
  NDATA="$DATA"; NLOG="$DIR/server.log"
  if [ -n "$EXE" ]; then NDATA="$(cygpath -m "$DATA")"; NLOG="$(cygpath -m "$NLOG")"; fi
  "$PG_BIN/initdb$EXE" -D "$NDATA" -U postgres --auth=trust -E UTF8 >/dev/null
  cp server.crt server.key ca.crt "$DATA/"
  chmod 600 "$DATA/server.key"
  SOCKET_LINE="unix_socket_directories = ''"
  SOCK=""
  if [ -z "$EXE" ]; then
    SOCK="${PG_SOCKET_DIR:-/tmp/kwpg-$PORT}"
    mkdir -p "$SOCK"
    SOCKET_LINE="unix_socket_directories = '$SOCK'"
  fi
  cat >> "$DATA/postgresql.conf" <<CONF
port = $PORT
listen_addresses = 'localhost'
ssl = on
ssl_cert_file = 'server.crt'
ssl_key_file = 'server.key'
ssl_ca_file = 'ca.crt'
$SOCKET_LINE
CONF
  "$PG_BIN/pg_ctl$EXE" -D "$NDATA" -l "$NLOG" -w start >/dev/null || { cat "$DIR/server.log"; exit 1; }
  # Roles while everything is still trusted, then the real access rules.
  "$PG_BIN/psql$EXE" -v ON_ERROR_STOP=1 -q -h 127.0.0.1 -p "$PORT" -U postgres -d postgres -f roles.sql >/dev/null
  "$PG_BIN/psql$EXE" -h 127.0.0.1 -p "$PORT" -U postgres -d postgres -tAc 'SELECT version()' >&2
  cp pg_hba.conf "$DATA/pg_hba.conf"
  "$PG_BIN/pg_ctl$EXE" -D "$NDATA" reload >/dev/null
else
  # ---- Docker -------------------------------------------------------------
  cat > init.sh <<'INIT'
#!/bin/sh
set -e
psql -v ON_ERROR_STOP=1 -q --username postgres -f /certs-src/roles.sql
cp /certs-src/pg_hba.conf "$PGDATA/pg_hba.conf"
INIT
  chmod 644 ./*.crt server.key ca.key init.sh roles.sql pg_hba.conf
  MOUNT=()
  SOCK=""
  if [ -n "${PG_SOCKET_DIR:-}" ]; then
    mkdir -p "$PG_SOCKET_DIR" && chmod 777 "$PG_SOCKET_DIR"
    MOUNT=(-v "$PG_SOCKET_DIR:/var/run/postgresql")
    SOCK="$PG_SOCKET_DIR"
  fi
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker create --name "$NAME" -p "$PORT:5432" ${MOUNT[@]+"${MOUNT[@]}"} -e POSTGRES_PASSWORD=postgres_pw "$IMAGE" sh -c '
    mkdir -p /certs && cp /certs-src/server.crt /certs-src/server.key /certs-src/ca.crt /certs/
    chown -R postgres /certs && chmod 600 /certs/server.key
    cp /certs-src/init.sh /docker-entrypoint-initdb.d/10-kernway.sh
    exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/certs/server.crt \
      -c ssl_key_file=/certs/server.key -c ssl_ca_file=/certs/ca.crt' >/dev/null
  docker cp "$DIR/." "$NAME:/certs-src"
  docker start "$NAME" >/dev/null
  # Ready once the final server (not the image's init-time one) takes TCP.
  for _ in $(seq 1 60); do
    if docker exec -e PGPASSWORD=kernway_pw "$NAME" psql -U kernway -d kernway_test -h 127.0.0.1 -tAc 'SELECT 1' >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  docker exec -e PGPASSWORD=kernway_pw "$NAME" psql -U kernway -d kernway_test -h 127.0.0.1 -tAc 'SELECT version()' >&2
fi

echo "KERNWAY_PG_URL=postgres://kernway:kernway_pw@127.0.0.1:$PORT/kernway_test"
# Native Windows programs read C:/… paths, not Git Bash's /c/….
if command -v cygpath >/dev/null 2>&1; then echo "KERNWAY_PG_TLS_DIR=$(cygpath -m "$DIR")"; else echo "KERNWAY_PG_TLS_DIR=$DIR"; fi
if [ -n "$SOCK" ]; then
  echo "KERNWAY_PG_SOCKET_DIR=$SOCK"
  echo "KERNWAY_PG_SOCKET_PORT=$([ -n "${PG_BIN:-}" ] && echo "$PORT" || echo 5432)"
fi
