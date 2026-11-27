#!/usr/bin/env bash
# A RabbitMQ broker for the client's tests:
#
#   amqp    127.0.0.1:5672   PLAIN / AMQPLAIN
#   amqps   localhost:5671   TLS (server cert for DNS:localhost from ca.pem);
#                            a client certificate is checked when sent, and
#                            EXTERNAL logs in as its CN (kw-client)
#   users   kw / kw_pw       administrator, vhosts "/" and "kw-vhost"
#           kw-client        (no password) for EXTERNAL, vhost "/"
#           kw-noaccess / na_pw  no vhost permissions
#
# Docker by default (RABBITMQ_IMAGE, default rabbitmq:4.1-management), or a
# broker installed natively (Homebrew / Chocolatey on the macOS / Windows CI
# runners) when RABBITMQ_SBIN names the dir with rabbitmq-server and
# rabbitmqctl: started in the background with this directory's config.
#
#   crates/kernway-rabbitmq/tests/support/rabbitmq-setup.sh [dir]
#
# Prints the test environment (KERNWAY_RABBITMQ_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/rabbitmq}"
IMAGE="${RABBITMQ_IMAGE:-rabbitmq:4.1-management}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"
EXE=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".bat" ;; esac
native_path() { if [ -n "$EXE" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
MODE=docker
if [ -n "${RABBITMQ_SBIN:-}" ]; then MODE=native; fi

# ---- certificates ----------------------------------------------------------------
export MSYS_NO_PATHCONV=1
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw-client" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null
unset MSYS_NO_PATHCONV
chmod 644 ./*.pem ./*.key ./*.crt

# ---- config --------------------------------------------------------------------------
if [ "$MODE" = docker ]; then CERTS=/etc/kw; else CERTS="$(native_path "$DIR")"; fi
cat > rabbitmq.conf <<EOF
listeners.tcp.default = 5672
listeners.ssl.default = 5671
ssl_options.cacertfile = $CERTS/ca.pem
ssl_options.certfile = $CERTS/server.crt
ssl_options.keyfile = $CERTS/server.key
ssl_options.verify = verify_peer
ssl_options.fail_if_no_peer_cert = false
ssl_options.versions.1 = tlsv1.3
ssl_options.versions.2 = tlsv1.2
auth_mechanisms.1 = PLAIN
auth_mechanisms.2 = AMQPLAIN
auth_mechanisms.3 = EXTERNAL
ssl_cert_login_from = common_name
loopback_users = none
EOF
echo '[rabbitmq_management,rabbitmq_auth_mechanism_ssl].' > enabled_plugins
chmod 644 rabbitmq.conf enabled_plugins

# ---- start ------------------------------------------------------------------------------
if [ "$MODE" = docker ]; then
  docker rm -f kernway-rabbitmq >/dev/null 2>&1 || true
  # The data dir on tmpfs, owned by the image's rabbitmq user (test data only).
  docker run -d --name kernway-rabbitmq --hostname kw-rmq -u 999:999 \
    --tmpfs /var/lib/rabbitmq:rw,uid=999,gid=999,mode=0700 \
    -v "$DIR":/etc/kw:ro -e RABBITMQ_CONFIG_FILE=/etc/kw/rabbitmq.conf -e RABBITMQ_ENABLED_PLUGINS_FILE=/etc/kw/enabled_plugins \
    -p 5672:5672 -p 5671:5671 -p 15672:15672 "$IMAGE" >/dev/null
  ctl() { docker exec kernway-rabbitmq rabbitmqctl "$@"; }
  CTL_CMD="docker exec kernway-rabbitmq rabbitmqctl"
else
  SBIN="$RABBITMQ_SBIN"
  export RABBITMQ_CONFIG_FILE="$(native_path "$DIR/rabbitmq.conf")"
  export RABBITMQ_ENABLED_PLUGINS_FILE="$(native_path "$DIR/enabled_plugins")"
  export RABBITMQ_MNESIA_BASE="$(native_path "$DIR/mnesia")"
  export RABBITMQ_LOG_BASE="$(native_path "$DIR/log")"
  export RABBITMQ_NODENAME="kw@localhost"
  rm -rf "$DIR/mnesia"
  mkdir -p "$DIR/mnesia" "$DIR/log"
  "$SBIN/rabbitmq-server$EXE" > "$DIR/server.out" 2>&1 &
  echo $! > "$DIR/pid"
  # MSYS_NO_PATHCONV: Git Bash would turn the vhost "/" into a Windows path.
  ctl() { MSYS_NO_PATHCONV=1 "$SBIN/rabbitmqctl$EXE" -n kw@localhost "$@"; }
  # Quoted: the install path may hold spaces (Program Files).
  CTL_CMD="MSYS_NO_PATHCONV=1 \"$SBIN/rabbitmqctl$EXE\" -n kw@localhost"
fi
for _ in $(seq 1 120); do
  if ctl await_startup >/dev/null 2>&1; then break; fi
  sleep 1
done
ctl await_startup >/dev/null

# ---- users / vhosts -----------------------------------------------------------------------
ctl add_user kw kw_pw >/dev/null 2>&1 || ctl change_password kw kw_pw >/dev/null
ctl set_user_tags kw administrator >/dev/null
ctl add_vhost kw-vhost >/dev/null 2>&1 || true
for v in / kw-vhost; do ctl set_permissions -p "$v" kw '.*' '.*' '.*' >/dev/null; done
ctl add_user kw-client unused_pw >/dev/null 2>&1 || true
ctl clear_password kw-client >/dev/null
ctl set_permissions -p / kw-client '.*' '.*' '.*' >/dev/null
ctl add_user kw-noaccess na_pw >/dev/null 2>&1 || true
ctl delete_user guest >/dev/null 2>&1 || true

echo "KERNWAY_RABBITMQ_URL=amqp://kw:kw_pw@127.0.0.1:5672/%2F"
echo "KERNWAY_RABBITMQ_DIR=$(native_path "$DIR")"
# A shell command prefix the tests run (bash -c) to drive the broker.
echo "KERNWAY_RABBITMQ_CTL=$CTL_CMD"
# The shell that runs it — on Windows, Git Bash (a bare `bash` there may be WSL's).
echo "KERNWAY_RABBITMQ_SHELL=$(native_path "$BASH")"
echo "KERNWAY_RABBITMQ_VERSION=$(ctl version | tr -d '\r' | tail -1)"
