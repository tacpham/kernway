#!/usr/bin/env bash
# Acceptance run for one database driver: bring up (or reuse) the database in
# Docker, prepare accounts / certificates, run every test of the driver one at
# a time against it, and write docs/acceptance/<db>.md.
#
#   acceptance/run.sh mysql
#   acceptance/run.sh oracle
#   acceptance/run.sh sqlite   (embedded: no container, real database files)
#
# Containers (created if missing; existing ones and their data are reused):
#   kernway-mysql   mysql:8.0                  host port 3307, root password "secret"
#   kernway-oracle  gvenzl/oracle-free:23-slim host ports 1521 / 2484 (TCPS)
# Oracle TCPS needs a wallet once: crates/kernway-orm-oracle/tests/support/oracle-tcps-setup.sh
set -euo pipefail
DB="${1:?usage: $0 mysql|oracle|sqlite}"
# CI workflow whose jobs for this commit are the cross-OS evidence.
WORKFLOW=drivers.yml; IMAGE=""
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
    SKIP=(--skip local_socket_)
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
    C=""; IMAGE="bundled with the driver (libsqlite3-sys, compiled from source)"; VERSION=""
    CRATE=kernway-orm-sqlite; FEATURES=(); TITLE=SQLite; SKIP=()
    # The workspace CI runs every crate's tests on Linux, macOS and Windows.
    WORKFLOW=workspace.yml
    ;;
  *) echo "unknown database: $DB (mysql | oracle | sqlite)" >&2; exit 2 ;;
esac

CMD="cargo test -p $CRATE ${FEATURES[*]+${FEATURES[*]}} --lib --tests -- --test-threads=1 --nocapture ${SKIP[*]+${SKIP[*]}}"
log "running: $CMD"
( cd "$ROOT" && cargo test -p "$CRATE" ${FEATURES[@]+"${FEATURES[@]}"} --lib --tests -- --test-threads=1 --nocapture ${SKIP[@]+"${SKIP[@]}"} ) > "$WORK/test.log" 2>&1 || true
if ! grep -q "^test result:" "$WORK/test.log"; then
  log "cargo test did not run any test binary:"; tail -20 "$WORK/test.log" >&2
fi

if [ "$DB" = sqlite ]; then
  VERSION="SQLite $(sed -n 's/^SQLite engine: //p' "$WORK/test.log" | head -1)"
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
if [ "$DB" = mysql ]; then
  in_container_tests connection local_socket_ \
    KERNWAY_MYSQL_SOCKET=/var/run/mysqld/mysqld.sock KERNWAY_MYSQL_ROOT_PASSWORD="$RPW"
  CMD="$CMD; local_socket_* inside $C"
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
grep -m3 -E '^## Verdict|^- Features|^- Tests' "$OUT/$DB.md" >&2
log "report: docs/acceptance/$DB.md"
exit $STATUS
