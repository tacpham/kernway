#!/usr/bin/env bash
# Acceptance run for one database driver: bring up (or reuse) the database in
# Docker, prepare accounts / certificates, run every test of the driver one at
# a time against it, and write docs/acceptance/<db>.md.
#
#   acceptance/run.sh mysql
#   acceptance/run.sh oracle
#
# Containers (created if missing; existing ones and their data are reused):
#   kernway-mysql   mysql:8.0                  host port 3307, root password "secret"
#   kernway-oracle  gvenzl/oracle-free:23-slim host ports 1521 / 2484 (TCPS)
# Oracle TCPS needs a wallet once: crates/kernway-orm-oracle/tests/support/oracle-tcps-setup.sh
set -euo pipefail
DB="${1:?usage: $0 mysql|oracle}"
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
    else
      log "no TCPS wallet in $C: the TCPS test will be reported as skipped (run oracle-tcps-setup.sh)"
    fi
    VERSION="$(docker exec "$C" bash -c "echo 'SET HEADING OFF
SET LINESIZE 400
SELECT banner_full FROM v\$version;' | sqlplus -s system/oracle_pw@//localhost:1521/FREEPDB1" | tr -s ' \n' ' ' | sed 's/^ //; s/ $//')"
    CRATE=kernway-orm-oracle; FEATURES=(); TITLE=Oracle
    ;;
  *) echo "unknown database: $DB (mysql | oracle)" >&2; exit 2 ;;
esac

CMD="cargo test -p $CRATE ${FEATURES[*]+${FEATURES[*]}} --lib --tests -- --test-threads=1 --nocapture"
log "running: $CMD"
( cd "$ROOT" && cargo test -p "$CRATE" ${FEATURES[@]+"${FEATURES[@]}"} --lib --tests -- --test-threads=1 --nocapture ) > "$WORK/test.log" 2>&1 || true
if ! grep -q "^test result:" "$WORK/test.log"; then
  log "cargo test did not run any test binary:"; tail -20 "$WORK/test.log" >&2
fi

python3 - "$WORK/meta.json" <<PY
import json, subprocess, sys, datetime
def sh(*a):
    try: return subprocess.check_output(a, cwd="$ROOT", text=True, stderr=subprocess.DEVNULL).strip()
    except Exception: return "?"
dirty = " (uncommitted changes)" if sh("git", "status", "--porcelain", "--", ".", ":!docs/acceptance") else ""
json.dump({
  "generated": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
  "server": """$VERSION""",
  "image": sh("docker", "inspect", "-f", "{{.Config.Image}}", "$C"),
  "container": "$C",
  "driver commit": sh("git", "rev-parse", "--short", "HEAD") + " on " + sh("git", "rev-parse", "--abbrev-ref", "HEAD") + dirty,
  "rustc": sh("rustc", "--version"),
  "command": "\`$CMD\`",
}, open(sys.argv[1], "w"))
PY

set +e
python3 "$ROOT/acceptance/report.py" "$TITLE" "$ROOT/acceptance/$DB.features" "$WORK/test.log" "$WORK/meta.json" > "$OUT/$DB.md"
STATUS=$?
set -e
grep -m3 -E '^## Verdict|^- Features|^- Tests' "$OUT/$DB.md" >&2
log "report: docs/acceptance/$DB.md"
exit $STATUS
