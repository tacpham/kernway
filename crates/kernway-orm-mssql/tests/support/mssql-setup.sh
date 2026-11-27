#!/usr/bin/env bash
# A SQL Server 2022 for the driver's tests:
#
#   127.0.0.1:1433   sa / Kernway_pw1!   kw_app / Kw_app_pw1! (owns kernway_test)
#
# TLS uses a fresh CA and a server certificate for DNS:localhost only (so
# 127.0.0.1 exercises the name check) and the container's host name, configured
# in mssql.conf.
#
#   crates/kernway-orm-mssql/tests/support/mssql-setup.sh [dir]
#
# Prints the test environment (KERNWAY_MSSQL_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/mssql}"
NAME="${MSSQL_CONTAINER:-kernway-mssql}"
IMAGE="${MSSQL_IMAGE:-mcr.microsoft.com/mssql/server:2022-latest}"
SA_PW='Kernway_pw1!'
# TLS 1.2: SQL Server speaks TLS 1.3 only over TDS 8.0 strict (whose server
# side refused this test certificate — see acceptance/mssql.features).
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"

openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
# Strict (TDS 8.0) connections need the server's own host name in the certificate too.
printf 'subjectAltName=DNS:localhost,DNS:kernway-mssql\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.pem 2>/dev/null
chmod 644 ./*.pem ./*.key
cat > mssql.conf <<CONF
[network]
tlscert = /var/opt/mssql/certs/server.pem
tlskey = /var/opt/mssql/certs/server.key
tlsprotocols = 1.2
forceencryption = 0
CONF

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker create --name "$NAME" --hostname kernway-mssql --platform linux/amd64 -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=$SA_PW" -p 1433:1433 "$IMAGE" >/dev/null
mkdir -p certs && cp server.pem server.key certs/ && chmod 755 certs && chmod 644 certs/*
docker cp "$DIR/mssql.conf" "$NAME:/var/opt/mssql/mssql.conf"
docker cp "$DIR/certs" "$NAME:/var/opt/mssql/certs"
docker start "$NAME" >/dev/null
SQLCMD=(docker exec "$NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P "$SA_PW" -b)
for _ in $(seq 1 120); do
  "${SQLCMD[@]}" -Q 'SELECT 1' >/dev/null 2>&1 && break
  if [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" != true ]; then docker logs "$NAME" 2>&1 | tail -5 >&2; exit 1; fi
  sleep 2
done
"${SQLCMD[@]}" -Q "
IF DB_ID('kernway_test') IS NOT NULL DROP DATABASE kernway_test;
CREATE DATABASE kernway_test;
IF SUSER_ID('kw_app') IS NULL CREATE LOGIN kw_app WITH PASSWORD = 'Kw_app_pw1!', CHECK_POLICY = OFF;
ALTER AUTHORIZATION ON DATABASE::kernway_test TO kw_app;
GRANT VIEW SERVER STATE TO kw_app;
" >/dev/null
"${SQLCMD[@]}" -Q 'SELECT @@VERSION' -h -1 | head -1 >&2
echo "KERNWAY_MSSQL_URL=mssql://kw_app:Kw_app_pw1%21@127.0.0.1:1433/kernway_test?trustServerCertificate=true"
echo "KERNWAY_MSSQL_SA_URL=mssql://sa:Kernway_pw1%21@127.0.0.1:1433/master?trustServerCertificate=true"
echo "KERNWAY_MSSQL_TLS_DIR=$DIR"

