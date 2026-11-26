#!/usr/bin/env bash
# Start a real MySQL 8.0 on the CI runner — its own build for the OS — with the
# local channel the driver tests use: a Unix socket file, or a named pipe on
# Windows. Prints the environment for the tests to $GITHUB_ENV.
#   Linux:   the mysql:8.0 service container (socket bind-mounted from the host)
#   macOS:   the official 8.0.40 macOS arm64 tarball
#   Windows: the official 8.0.40 winx64 zip, with --named-pipe
set -euo pipefail
VER=8.0.40
ROOT_PW=secret

setup_sql="
CREATE USER IF NOT EXISTS 'root'@'%' IDENTIFIED WITH mysql_native_password BY '$ROOT_PW';
GRANT ALL ON *.* TO 'root'@'%' WITH GRANT OPTION;
CREATE DATABASE IF NOT EXISTS kernway_test;
CREATE USER IF NOT EXISTS 'kernway'@'%' IDENTIFIED WITH mysql_native_password BY 'kernway_pw';
GRANT ALL ON kernway_test.* TO 'kernway'@'%';
CREATE USER IF NOT EXISTS 'kw_sha2'@'%' IDENTIFIED WITH caching_sha2_password BY 'sha2_pw';
GRANT ALL ON kernway_test.* TO 'kw_sha2'@'%';
SET GLOBAL log_bin_trust_function_creators = 1;
SET GLOBAL local_infile = 1;
"

case "$RUNNER_OS" in
  Linux)
    # The service container already runs; only accounts are missing.
    docker exec -i "$MYSQL_CONTAINER" mysql -uroot -p"$ROOT_PW" <<<"$setup_sql"
    CHANNEL=/tmp/mysqld/mysqld.sock
    ;;
  macOS|Windows)
    T="${RUNNER_TEMP:-/tmp}"
    if [ "$RUNNER_OS" = macOS ]; then
      PKG="mysql-$VER-macos14-arm64"
      curl -fsSL "https://cdn.mysql.com/archives/mysql-8.0/$PKG.tar.gz" | tar xz -C "$T"
      BIN="$T/$PKG/bin"; EXE=""
      DATA="$T/kwdata"; CHANNEL=/tmp/kw-mysql.sock
      LOCAL=(--socket="$CHANNEL")
    else
      PKG="mysql-$VER-winx64"
      curl -fsSL -o "$T/mysql.zip" "https://cdn.mysql.com/archives/mysql-8.0/$PKG.zip"
      unzip -q "$T/mysql.zip" -d "$T"
      BIN="$T/$PKG/bin"; EXE=".exe"
      DATA="$(cygpath -m "$T")/kwdata"; CHANNEL=MySQL
      LOCAL=(--named-pipe --socket="$CHANNEL")
    fi
    "$BIN/mysqld$EXE" --initialize-insecure --datadir="$DATA" --console
    "$BIN/mysqld$EXE" --datadir="$DATA" --port=3306 --mysqlx=OFF "${LOCAL[@]}" --console > "$T/mysqld.log" 2>&1 &
    for _ in $(seq 1 60); do
      if "$BIN/mysqladmin$EXE" -uroot -h127.0.0.1 --protocol=TCP ping >/dev/null 2>&1; then break; fi
      sleep 2
    done
    "$BIN/mysql$EXE" -uroot -h127.0.0.1 --protocol=TCP <<<"ALTER USER 'root'@'localhost' IDENTIFIED WITH mysql_native_password BY '$ROOT_PW'; $setup_sql" \
      || { cat "$T/mysqld.log"; exit 1; }
    ;;
esac

"${BIN:+$BIN/}mysql${EXE:-}" --version 2>/dev/null || true
{
  echo "KERNWAY_MYSQL_URL=mysql://kernway:kernway_pw@127.0.0.1:3306/kernway_test"
  echo "KERNWAY_MYSQL_ROOT_URL=mysql://root:$ROOT_PW@127.0.0.1:3306/kernway_test"
  echo "KERNWAY_MYSQL_SHA2_URL=mysql://kw_sha2:sha2_pw@127.0.0.1:3306/kernway_test"
  echo "KERNWAY_MYSQL_SOCKET=$CHANNEL"
  echo "KERNWAY_MYSQL_ROOT_PASSWORD=$ROOT_PW"
} >> "$GITHUB_ENV"
