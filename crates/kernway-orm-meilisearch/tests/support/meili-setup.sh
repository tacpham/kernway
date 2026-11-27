#!/usr/bin/env bash
# Meilisearch for the driver's tests: the official release binary, run
# natively on every OS (downloaded when MEILI_BIN is not set), on
# 127.0.0.1:7700 with master key `testmasterkey` and a fresh data dir.
#
#   crates/kernway-orm-meilisearch/tests/support/meili-setup.sh [dir]
#
# Prints the test environment (MEILI_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/meili}"
VERSION="${MEILI_VERSION:-v1.11.3}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
WIN=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; esac
native_path() { if [ -n "$WIN" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

if [ -z "${MEILI_BIN:-}" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset=meilisearch-linux-amd64 ;;
    Linux-aarch64) asset=meilisearch-linux-aarch64 ;;
    Darwin-arm64) asset=meilisearch-macos-apple-silicon ;;
    Darwin-x86_64) asset=meilisearch-macos-amd64 ;;
    MINGW*|MSYS*|CYGWIN*) asset=meilisearch-windows-amd64.exe ;;
    *) echo "no Meilisearch build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
  esac
  MEILI_BIN="$DIR/$asset"
  if [ ! -x "$MEILI_BIN" ]; then
    curl -fsSL -o "$MEILI_BIN" "https://github.com/meilisearch/meilisearch/releases/download/$VERSION/$asset"
    chmod +x "$MEILI_BIN"
  fi
fi

if [ -f "$DIR/pid" ]; then kill "$(cat "$DIR/pid")" 2>/dev/null || true; sleep 1; fi
rm -rf "$DIR/data"
"$MEILI_BIN" --db-path "$(native_path "$DIR/data")" --dump-dir "$(native_path "$DIR/dumps")" \
  --http-addr 127.0.0.1:7700 --master-key testmasterkey --no-analytics --env development > "$DIR/server.out" 2>&1 &
echo $! > "$DIR/pid"
for _ in $(seq 1 60); do
  if curl -fs http://127.0.0.1:7700/health >/dev/null 2>&1; then break; fi
  sleep 0.5
done
curl -fs http://127.0.0.1:7700/health >/dev/null || { echo "Meilisearch did not start; see $DIR/server.out" >&2; exit 1; }

echo "MEILI_URL=http://127.0.0.1:7700"
echo "MEILI_API_KEY=testmasterkey"
echo "MEILI_DIR=$(native_path "$DIR")"
echo "MEILI_VERSION=$("$MEILI_BIN" --version | tr -d '\r' | sed 's/^meilisearch //')"
