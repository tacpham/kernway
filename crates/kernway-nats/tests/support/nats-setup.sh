#!/usr/bin/env bash
# NATS servers for the client's tests (native nats-server on every OS — one
# static binary, downloaded for this OS when NATS_SERVER is not set):
#
#   plain       127.0.0.1:4222   JetStream on (store in <dir>/js)
#   auth        127.0.0.1:4223   users kw / kw_pw and the docs NKey
#                                UDXU4RCSJNZOIQHZNWXHXORDPRTGNJAHAHFRGZNEEJCPQTT2M7NLCNF4
#   token       127.0.0.1:4224   token s3cr3t
#   tls         localhost:4225   TLS required (cert for DNS:localhost, ca.pem)
#   tls_first   localhost:4226   TLS before INFO (handshake_first)
#   mtls        localhost:4227   TLS + a client certificate (client.crt/key)
#   cluster     127.0.0.1:4230-4232  three routed nodes, JetStream, each
#                                advertising 127.0.0.1:<its port>
#
#   crates/kernway-nats/tests/support/nats-setup.sh [dir]
#
# Prints the test environment (KERNWAY_NATS_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/nats}"
VERSION="${NATS_VERSION:-2.10.29}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"
EXE=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
native_path() { if [ -n "$EXE" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# ---- the server binary ---------------------------------------------------------
if [ -z "${NATS_SERVER:-}" ]; then
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *) echo "unsupported OS $(uname -s)" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=amd64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
  esac
  name="nats-server-v$VERSION-$os-$arch"
  if [ ! -x "$DIR/$name/nats-server$EXE" ]; then
    base="https://github.com/nats-io/nats-server/releases/download/v$VERSION/$name"
    if [ "$os" = windows ]; then
      curl -fsSL -o "$DIR/nats.zip" "$base.zip" && unzip -qo "$DIR/nats.zip" -d "$DIR"
    else
      curl -fsSL "$base.tar.gz" | tar xz -C "$DIR"
    fi
  fi
  NATS_SERVER="$DIR/$name/nats-server$EXE"
fi

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
P() { native_path "$DIR/$1"; }

# ---- configs -----------------------------------------------------------------------
cat > plain.conf <<EOF
listen: 127.0.0.1:4222
jetstream { store_dir: "$(P js-plain)" }
EOF
cat > auth.conf <<EOF
listen: 127.0.0.1:4223
authorization {
  users: [
    { user: kw, password: kw_pw }
    { nkey: UDXU4RCSJNZOIQHZNWXHXORDPRTGNJAHAHFRGZNEEJCPQTT2M7NLCNF4 }
  ]
}
EOF
cat > token.conf <<EOF
listen: 127.0.0.1:4224
authorization { token: s3cr3t }
EOF
cat > tls.conf <<EOF
listen: 127.0.0.1:4225
tls { cert_file: "$(P server.crt)", key_file: "$(P server.key)" }
EOF
cat > tls_first.conf <<EOF
listen: 127.0.0.1:4226
tls { cert_file: "$(P server.crt)", key_file: "$(P server.key)", handshake_first: true }
EOF
cat > mtls.conf <<EOF
listen: 127.0.0.1:4227
tls { cert_file: "$(P server.crt)", key_file: "$(P server.key)", ca_file: "$(P ca.pem)", verify: true }
EOF
for i in 0 1 2; do
  port=$((4230 + i))
  cat > "cluster$i.conf" <<EOF
server_name: kw-n$i
listen: 127.0.0.1:$port
client_advertise: "127.0.0.1:$port"
jetstream { store_dir: "$(P js-n$i)" }
cluster {
  name: kw-cluster
  listen: 127.0.0.1:$((6230 + i))
  routes: [ nats://127.0.0.1:6230, nats://127.0.0.1:6231, nats://127.0.0.1:6232 ]
}
EOF
done

# ---- start -------------------------------------------------------------------------
stop_old() {
  if [ -f "$DIR/pids" ]; then
    while read -r pid; do kill "$pid" 2>/dev/null || true; done < "$DIR/pids"
    rm -f "$DIR/pids"
    sleep 1
  fi
}
stop_old
rm -rf js-plain js-n0 js-n1 js-n2
for c in plain auth token tls tls_first mtls cluster0 cluster1 cluster2; do
  "$NATS_SERVER" -c "$(P "$c.conf")" > "$DIR/$c.log" 2>&1 &
  echo $! >> "$DIR/pids"
done
wait_port() {
  for _ in $(seq 1 60); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then return 0; fi
    sleep 0.5
  done
  echo "nats-server on $1 did not start; logs in $DIR" >&2
  exit 1
}
for p in 4222 4223 4224 4225 4226 4227 4230 4231 4232; do wait_port $p; done
# JetStream in the cluster needs a meta leader before streams can be made.
sleep 3

echo "KERNWAY_NATS_URL=nats://127.0.0.1:4222"
echo "KERNWAY_NATS_DIR=$(native_path "$DIR")"
echo "KERNWAY_NATS_VERSION=$("$NATS_SERVER" --version | tr -d '\r' | sed 's/.*: //')"
