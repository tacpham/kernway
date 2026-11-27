#!/usr/bin/env bash
# A TLS-only Redis for the driver's TLS tests: a fresh CA, a server
# certificate for DNS:localhost only (so 127.0.0.1 exercises name checking),
# and a client certificate for mutual TLS. Client certificates are optional
# on the server; the test switches them to required at runtime.
#
#   crates/kernway-redis/tests/support/redis-tls-setup.sh [dir]
#
# Creates (or recreates) container kernway-redis-tls on host port 6381 and
# prints the environment the tests read.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/redis-tls}"
NAME="${REDIS_TLS_CONTAINER:-kernway-redis-tls}"
PORT="${REDIS_TLS_PORT:-6381}"
IMAGE="${REDIS_IMAGE:-redis:7.4-alpine}"
mkdir -p "$DIR"
cd "$DIR"

openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" \
  -keyout ca.key -out ca.crt >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr >/dev/null 2>&1
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile server.ext -out server.crt >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj "/CN=kernway-client" -keyout client.key -out client.csr >/dev/null 2>&1
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile client.ext -out client.crt >/dev/null 2>&1
chmod 644 ./*.crt ./*.key

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker create --name "$NAME" -p "$PORT:6379" "$IMAGE" redis-server \
  --port 0 --tls-port 6379 \
  --tls-cert-file /tls/server.crt --tls-key-file /tls/server.key \
  --tls-ca-cert-file /tls/ca.crt --tls-auth-clients optional >/dev/null
docker cp "$DIR/." "$NAME:/tls"
docker start "$NAME" >/dev/null
for _ in $(seq 1 30); do
  if docker exec "$NAME" redis-cli --tls -p 6379 --cacert /tls/ca.crt --sni localhost PING 2>/dev/null | grep -q PONG; then break; fi
  sleep 1
done
echo "export KERNWAY_REDIS_TLS_URL=rediss://localhost:$PORT"
echo "export KERNWAY_REDIS_TLS_DIR=$DIR"
