#!/usr/bin/env bash
# MariaDB for the MySQL driver's tests (the driver speaks to MariaDB as to
# MySQL; this proves it against the real server):
#
#   127.0.0.1:3308   MariaDB (MARIADB_IMAGE, default mariadb:11.8 — the LTS)
#                    root / secret; kernway / kernway_pw owning kernway_test
#                    (mysql_native_password, MariaDB's default); the
#                    auth_ed25519 and auth_parsec plugins installed
#                    max_allowed_packet 64M (MySQL 8's default; MariaDB's is 16M)
#   TLS              server certificate from this directory's test CA
#                    (ca.pem); client-cert.pem / client-key.pem for X509 users
#
#   crates/kernway-orm-mysql/tests/support/mariadb-setup.sh [dir]
#
# Prints the test environment (KERNWAY_MYSQL_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/mariadb}"
IMAGE="${MARIADB_IMAGE:-mariadb:11.8}"
C="${MARIADB_CONTAINER:-kernway-mariadb}"
PORT="${MARIADB_PORT:-3308}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"

export MSYS_NO_PATHCONV=1
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
# DNS:localhost only: connecting to 127.0.0.1 with verify_identity must fail.
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw-client" -keyout client-key.pem -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client-cert.pem 2>/dev/null
unset MSYS_NO_PATHCONV
chmod 644 ./*.pem ./*.key ./*.crt

docker rm -f "$C" >/dev/null 2>&1 || true
docker run -d --name "$C" -p "$PORT:3306" -e MARIADB_ROOT_PASSWORD=secret -v "$DIR":/etc/kw:ro "$IMAGE" \
  --ssl-ca=/etc/kw/ca.pem --ssl-cert=/etc/kw/server.crt --ssl-key=/etc/kw/server.key \
  --max-allowed-packet=64M >/dev/null
for _ in $(seq 1 90); do
  docker exec "$C" mariadb -uroot -psecret -h127.0.0.1 --protocol=TCP -e 'SELECT 1' >/dev/null 2>&1 && break
  sleep 2
done
docker exec -i "$C" mariadb -uroot -psecret <<SQL
CREATE DATABASE IF NOT EXISTS kernway_test;
CREATE USER IF NOT EXISTS 'kernway'@'%' IDENTIFIED BY 'kernway_pw';
GRANT ALL ON kernway_test.* TO 'kernway'@'%';
GRANT ALL ON *.* TO 'root'@'%';
SQL
# The Ed25519 plugins (the tests create their accounts).
for p in auth_ed25519 auth_parsec; do
  docker exec "$C" mariadb -uroot -psecret -N -e "SELECT 1 FROM information_schema.plugins WHERE plugin_library LIKE '$p%'" | grep -q 1 \
    || docker exec "$C" mariadb -uroot -psecret -e "INSTALL SONAME '$p'"
done

echo "KERNWAY_MYSQL_URL=mysql://kernway:kernway_pw@127.0.0.1:$PORT/kernway_test"
echo "KERNWAY_MYSQL_ROOT_URL=mysql://root:secret@127.0.0.1:$PORT/kernway_test"
echo "KERNWAY_MYSQL_CA=$DIR/ca.pem"
echo "KERNWAY_MYSQL_CERTS=$DIR"
echo "KERNWAY_MARIADB_VERSION=$(docker exec "$C" mariadb -uroot -psecret -N -e 'SELECT VERSION()')"
