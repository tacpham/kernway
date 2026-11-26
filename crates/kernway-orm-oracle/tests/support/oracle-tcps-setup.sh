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
set -euo pipefail
OUT="${1:?usage: $0 <wallet output dir>}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
docker run --rm --entrypoint bash -v "$OUT:/out" gvenzl/oracle-free:23-full -c '
  set -e; PW="Kernway#Wallet1"; W=/tmp/w; mkdir -p $W
  orapki wallet create -wallet $W -pwd "$PW" -auto_login >/dev/null
  orapki wallet add -wallet $W -pwd "$PW" -dn "CN=localhost" -keysize 2048 -self_signed \
    -validity 3650 -sign_alg sha256 -addext_san "DNS:localhost,IPV4Address:127.0.0.1" >/dev/null
  orapki wallet export -wallet $W -pwd "$PW" -dn "CN=localhost" -cert $W/server.crt >/dev/null
  cp $W/cwallet.sso $W/ewallet.p12 $W/server.crt /out/'
docker exec kernway-oracle mkdir -p /opt/oracle/wallet
for f in cwallet.sso ewallet.p12 server.crt; do docker cp "$OUT/$f" "kernway-oracle:/opt/oracle/wallet/$f"; done
docker exec -u 0 kernway-oracle bash -c 'chown -R oracle:oinstall /opt/oracle/wallet && chmod 600 /opt/oracle/wallet/*'
docker exec -i kernway-oracle bash <<'IN'
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
SSL_CLIENT_AUTHENTICATION = FALSE
SQN
lsnrctl stop >/dev/null 2>&1 || true
lsnrctl start | grep -iE "tcps|error"
echo "ALTER SYSTEM REGISTER;" | sqlplus -s / as sysdba >/dev/null
IN
