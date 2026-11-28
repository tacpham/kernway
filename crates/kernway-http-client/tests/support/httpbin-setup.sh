#!/usr/bin/env bash
# Real HTTP servers for the client's live tests:
#
#   http://127.0.0.1:18080    go-httpbin (plain)
#   https://localhost:18443   go-httpbin over TLS, certificate from a fresh test CA
#   https://localhost:18444   Caddy requiring a client certificate from that CA (mTLS)
#   http://127.0.0.1:18445    Caddy serving <dir>/www, precompressed .br / .gz files
#
# go-httpbin (MIT) answers every HTTP feature a client needs proving — redirects,
# cookies, gzip / deflate, chunked bodies with trailers, delays, auth — and Caddy
# (Apache-2.0) is the mutual-TLS server and the brotli one (go-httpbin answers
# /brotli with 501). Both are official release binaries, run
# natively (downloaded when HTTPBIN_BIN / CADDY_BIN are not set), or in Docker
# with HTTPBIN_DOCKER=1.
#
#   crates/kernway-http-client/tests/support/httpbin-setup.sh [dir]
#
# Prints the test environment (KERNWAY_HTTPBIN_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/httpbin}"
HTTPBIN_VERSION="${HTTPBIN_VERSION:-2.26.0}"
CADDY_VERSION="${CADDY_VERSION:-2.11.9}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
WIN=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; esac
native_path() { if [ -n "$WIN" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# A test CA; the server certificate names localhost, 127.0.0.1 and ::1.
cd "$DIR"
export MSYS_NO_PATHCONV=1
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw-client" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null
# A second CA nobody trusts, for the "unknown CA" case.
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=untrusted CA" -keyout other-ca.key -out other-ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=intruder" -keyout intruder.key -out intruder.csr 2>/dev/null
openssl x509 -req -in intruder.csr -CA other-ca.pem -CAkey other-ca.key -CAcreateserial -days 30 -extfile client.ext -out intruder.crt 2>/dev/null
unset MSYS_NO_PATHCONV
chmod 644 ./*.pem ./*.key ./*.crt

# Caddy: mTLS only, answering with the client certificate's subject.
CERTS="$(native_path "$DIR")"; [ -n "${HTTPBIN_DOCKER:-}" ] && CERTS=/kw
cat > Caddyfile <<EOF
{
	auto_https off
	admin off
	servers {
		protocols h1
	}
}
https://localhost:18444, https://127.0.0.1:18444 {
	tls $CERTS/server.crt $CERTS/server.key {
		client_auth {
			mode require_and_verify
			trust_pool file $CERTS/ca.pem
		}
	}
	respond "client {http.request.tls.client.subject}"
}
http://127.0.0.1:18445, http://localhost:18445 {
	root * $CERTS/www
	file_server {
		precompressed br gzip
	}
}
EOF
rm -rf www; mkdir -p www; chmod 777 www

stop() { if [ -f "$DIR/$1.pid" ]; then kill "$(cat "$DIR/$1.pid")" 2>/dev/null || true; rm -f "$DIR/$1.pid"; sleep 1; fi; }
wait_port() { # port name log
  for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null; then return; fi
    sleep 0.5
  done
  echo "$2 did not start; see $3" >&2; cat "$3" >&2 2>/dev/null || true; exit 1
}

if [ -n "${HTTPBIN_DOCKER:-}" ]; then
  for c in kernway-httpbin kernway-httpbin-tls kernway-caddy; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  docker run -d --name kernway-httpbin -p 18080:8080 "ghcr.io/mccutchen/go-httpbin:$HTTPBIN_VERSION" >/dev/null
  docker run -d --name kernway-httpbin-tls -p 18443:8443 -v "$DIR":/kw:ro "ghcr.io/mccutchen/go-httpbin:$HTTPBIN_VERSION" \
    -port 8443 -https-cert-file /kw/server.crt -https-key-file /kw/server.key >/dev/null
  docker run -d --name kernway-caddy -p 18444:18444 -p 18445:18445 -v "$DIR":/kw "caddy:$CADDY_VERSION" caddy run --config /kw/Caddyfile --adapter caddyfile >/dev/null
  SERVER="go-httpbin $HTTPBIN_VERSION, Caddy $CADDY_VERSION (Docker)"
else
  os_arch="$(uname -s)-$(uname -m)"
  if [ -z "${HTTPBIN_BIN:-}" ]; then
    case "$os_arch" in
      Linux-x86_64) asset=go-httpbin-linux-amd64.tar.gz ;;
      Linux-aarch64) asset=go-httpbin-linux-arm64.tar.gz ;;
      Darwin-*) asset=go-httpbin-darwin-all.tar.gz ;;
      MINGW*|MSYS*|CYGWIN*) asset=go-httpbin-windows-amd64.zip ;;
      *) echo "no go-httpbin build for $os_arch" >&2; exit 1 ;;
    esac
    mkdir -p "$DIR/httpbin-bin"
    HTTPBIN_BIN="$(find "$DIR/httpbin-bin" -type f -name 'go-httpbin*' ! -name '*.json' | sed -n 1p)"
    if [ -z "$HTTPBIN_BIN" ]; then
      url="https://github.com/mccutchen/go-httpbin/releases/download/v$HTTPBIN_VERSION/$asset"
      if [ -n "$WIN" ]; then
        curl -fsSL -o "$DIR/httpbin.zip" "$url" && unzip -qo "$DIR/httpbin.zip" -d "$DIR/httpbin-bin"
      else
        curl -fsSL "$url" | tar xz -C "$DIR/httpbin-bin"
      fi
      HTTPBIN_BIN="$(find "$DIR/httpbin-bin" -type f -name 'go-httpbin*' ! -name '*.json' | sed -n 1p)"
      chmod +x "$HTTPBIN_BIN"
    fi
  fi
  if [ -z "${CADDY_BIN:-}" ]; then
    case "$os_arch" in
      Linux-x86_64) asset="caddy_${CADDY_VERSION}_linux_amd64.tar.gz" ;;
      Linux-aarch64) asset="caddy_${CADDY_VERSION}_linux_arm64.tar.gz" ;;
      Darwin-arm64) asset="caddy_${CADDY_VERSION}_mac_arm64.tar.gz" ;;
      Darwin-x86_64) asset="caddy_${CADDY_VERSION}_mac_amd64.tar.gz" ;;
      MINGW*|MSYS*|CYGWIN*) asset="caddy_${CADDY_VERSION}_windows_amd64.zip" ;;
      *) echo "no Caddy build for $os_arch" >&2; exit 1 ;;
    esac
    mkdir -p "$DIR/caddy-bin"
    CADDY_BIN="$DIR/caddy-bin/caddy"; [ -n "$WIN" ] && CADDY_BIN="$CADDY_BIN.exe"
    if [ ! -x "$CADDY_BIN" ]; then
      url="https://github.com/caddyserver/caddy/releases/download/v$CADDY_VERSION/$asset"
      if [ -n "$WIN" ]; then
        curl -fsSL -o "$DIR/caddy.zip" "$url" && unzip -qo "$DIR/caddy.zip" -d "$DIR/caddy-bin"
      else
        curl -fsSL "$url" | tar xz -C "$DIR/caddy-bin"
      fi
    fi
  fi
  stop httpbin; stop httpbin-tls; stop caddy
  "$HTTPBIN_BIN" -host :: -port 18080 > "$DIR/httpbin.out" 2>&1 &
  echo $! > "$DIR/httpbin.pid"
  "$HTTPBIN_BIN" -host :: -port 18443 -https-cert-file "$(native_path "$DIR/server.crt")" \
    -https-key-file "$(native_path "$DIR/server.key")" > "$DIR/httpbin-tls.out" 2>&1 &
  echo $! > "$DIR/httpbin-tls.pid"
  "$CADDY_BIN" run --config "$(native_path "$DIR/Caddyfile")" --adapter caddyfile > "$DIR/caddy.out" 2>&1 &
  echo $! > "$DIR/caddy.pid"
  SERVER="go-httpbin $HTTPBIN_VERSION, Caddy $CADDY_VERSION (native)"
fi
wait_port 18080 go-httpbin "$DIR/httpbin.out"
wait_port 18443 "go-httpbin (TLS)" "$DIR/httpbin-tls.out"
wait_port 18444 Caddy "$DIR/caddy.out"
wait_port 18445 "Caddy (files)" "$DIR/caddy.out"

echo "KERNWAY_HTTPBIN_URL=http://127.0.0.1:18080"
echo "KERNWAY_HTTPBIN_TLS_URL=https://localhost:18443"
echo "KERNWAY_HTTPBIN_MTLS_URL=https://localhost:18444"
echo "KERNWAY_HTTPBIN_FILES_URL=http://127.0.0.1:18445"
echo "KERNWAY_HTTPBIN_CERTS=$(native_path "$DIR")"
echo "KERNWAY_HTTPBIN_SERVER=$SERVER"
