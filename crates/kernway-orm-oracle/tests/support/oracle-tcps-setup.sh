#!/usr/bin/env bash
# Enable TCPS (TLS) on port 2484 in a gvenzl/oracle-free container, for the
# live TLS tests. Run from the host.
#
# The slim image has no orapki, so the auto-login wallet (CN=localhost, SAN
# localhost / 127.0.0.1) is created in a throw-away container of the full
# image (no database is started there), then copied in.
#
#   docker run -d --name kernway-oracle -p 1521:1521 -p 2484:2484 \
#     -e ORACLE_PASSWORD=oracle_pw -e APP_USER=kernway -e APP_USER_PASSWORD=kernway_pw \
#     gvenzl/oracle-free:23-slim
#   # wait for "DATABASE IS READY TO USE!" in `docker logs kernway-oracle`, then:
#   crates/kernway-orm-oracle/tests/support/oracle-tcps-setup.sh ./oracle-wallet
#   KERNWAY_ORACLE_HOST=127.0.0.1 KERNWAY_ORACLE_CA=./oracle-wallet/server.crt \
#     cargo test -p kernway-orm-oracle
#
# With --mtls the server also requires a client certificate
# (SSL_CLIENT_AUTHENTICATION = TRUE): a client key / certificate pair is
# made with openssl (client.key / client.crt / client.pem in the output dir,
# also copied to /opt/oracle/wallet/client/ for the acceptance run) and
# trusted by the server wallet.
set -euo pipefail
MTLS=false
if [ "${1:-}" = "--mtls" ]; then MTLS=true; shift; fi
OUT="${1:?usage: $0 [--mtls] <wallet output dir>}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
TRUST_CLIENT=""
if $MTLS; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=kernway-client" \
    -keyout "$OUT/client.key" -out "$OUT/client.crt" 2>/dev/null
  cat "$OUT/client.key" "$OUT/client.crt" > "$OUT/client.pem"
  TRUST_CLIENT='orapki wallet add -wallet $W -pwd "$PW" -trusted_cert -cert /out/client.crt >/dev/null'
fi
docker run --rm --entrypoint bash -v "$OUT:/out" gvenzl/oracle-free:23-full -c '
  set -e; PW="Kernway#Wallet1"; W=/tmp/w; mkdir -p $W
  orapki wallet create -wallet $W -pwd "$PW" -auto_login >/dev/null
  orapki wallet add -wallet $W -pwd "$PW" -dn "CN=localhost" -keysize 2048 -self_signed \
    -validity 3650 -sign_alg sha256 -addext_san "DNS:localhost,IPV4Address:127.0.0.1" >/dev/null
  orapki wallet export -wallet $W -pwd "$PW" -dn "CN=localhost" -cert $W/server.crt >/dev/null
  '"$TRUST_CLIENT"'
  cp $W/cwallet.sso $W/ewallet.p12 $W/server.crt /out/'
docker exec kernway-oracle mkdir -p /opt/oracle/wallet
for f in cwallet.sso ewallet.p12 server.crt; do docker cp "$OUT/$f" "kernway-oracle:/opt/oracle/wallet/$f"; done
if $MTLS; then
  docker exec kernway-oracle mkdir -p /opt/oracle/wallet/client
  for f in client.key client.crt client.pem; do docker cp "$OUT/$f" "kernway-oracle:/opt/oracle/wallet/client/$f"; done
fi
CLIENT_AUTH=$($MTLS && echo TRUE || echo FALSE)
docker exec -u 0 kernway-oracle bash -c 'chown -R oracle:oinstall /opt/oracle/wallet && chmod 600 /opt/oracle/wallet/*.* 2>/dev/null; true'
docker exec -i -e CLIENT_AUTH="$CLIENT_AUTH" kernway-oracle bash <<'IN'
set -e
A=$ORACLE_HOME/network/admin
cat > $A/listener.ora <<LSN
LISTENER =
  (DESCRIPTION_LIST =
    (DESCRIPTION =
      (ADDRESS = (PROTOCOL = IPC)(KEY = EXTPROC_FOR_FREE))
      (ADDRESS = (PROTOCOL = TCP)(HOST =)(PORT = 1521))
      (ADDRESS = (PROTOCOL = TCPS)(HOST =)(PORT = 2484))
    )
  )
DEFAULT_SERVICE_LISTENER = FREE
WALLET_LOCATION = (SOURCE = (METHOD = FILE)(METHOD_DATA = (DIRECTORY = /opt/oracle/wallet)))
SSL_CLIENT_AUTHENTICATION = FALSE
LSN
grep -q WALLET_LOCATION $A/sqlnet.ora || cat >> $A/sqlnet.ora <<SQN
WALLET_LOCATION = (SOURCE = (METHOD = FILE)(METHOD_DATA = (DIRECTORY = /opt/oracle/wallet)))
SSL_CLIENT_AUTHENTICATION = $CLIENT_AUTH
SQN
sed -i "s/^SSL_CLIENT_AUTHENTICATION = .*/SSL_CLIENT_AUTHENTICATION = $CLIENT_AUTH/" $A/sqlnet.ora
lsnrctl stop >/dev/null 2>&1 || true
lsnrctl start | grep -iE "tcps|error"
echo "ALTER SYSTEM REGISTER;" | sqlplus -s / as sysdba >/dev/null
IN
