#!/usr/bin/env bash
# An S3-compatible server for the client's tests: the Versity S3 Gateway
# (versitygw, Apache-2.0 — MinIO no longer publishes its community builds),
# run natively on every OS over a fresh posix directory:
#
#   http://127.0.0.1:9000   region us-east-1, access key kwaccess,
#                           secret kwsecret-0123456789abcdef
#
# Downloaded when VERSITYGW_BIN is not set.
#
#   crates/kernway-s3/tests/support/s3-setup.sh [dir]
#
# Prints the test environment (KERNWAY_S3_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/s3}"
VERSION="${VERSITYGW_VERSION:-1.11.0}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
WIN=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; esac
native_path() { if [ -n "$WIN" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

if [ -z "${VERSITYGW_BIN:-}" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset="versitygw_v${VERSION}_Linux_x86_64.tar.gz" ;;
    Linux-aarch64) asset="versitygw_v${VERSION}_Linux_arm64.tar.gz" ;;
    Darwin-arm64) asset="versitygw_v${VERSION}_Darwin_arm64.tar.gz" ;;
    Darwin-x86_64) asset="versitygw_v${VERSION}_Darwin_x86_64.tar.gz" ;;
    MINGW*|MSYS*|CYGWIN*) asset="versitygw_v${VERSION}_Windows_x86_64.zip" ;;
    *) echo "no versitygw build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
  esac
  name="${asset%.tar.gz}"; name="${name%.zip}"
  VERSITYGW_BIN="$DIR/$name/versitygw"; [ -n "$WIN" ] && VERSITYGW_BIN="$VERSITYGW_BIN.exe"
  if [ ! -x "$VERSITYGW_BIN" ]; then
    url="https://github.com/versity/versitygw/releases/download/v$VERSION/$asset"
    if [ -n "$WIN" ]; then
      curl -fsSL -o "$DIR/vgw.zip" "$url" && unzip -qo "$DIR/vgw.zip" -d "$DIR/$name"
      found="$(find "$DIR/$name" -name 'versitygw.exe' | sed -n 1p)"; VERSITYGW_BIN="$found"
    else
      curl -fsSL "$url" | tar xz -C "$DIR"
    fi
  fi
fi

if [ -f "$DIR/pid" ]; then kill "$(cat "$DIR/pid")" 2>/dev/null || true; sleep 1; fi
rm -rf "$DIR/data" "$DIR/meta"; mkdir -p "$DIR/data" "$DIR/meta"
# Object metadata in extended attributes — or, where the file system has
# none (Windows; VGW_SIDECAR=1 anywhere), in a sidecar directory.
META=()
if [ -n "$WIN" ] || [ -n "${VGW_SIDECAR:-}" ]; then META=(--sidecar "$(native_path "$DIR/meta")"); fi
"$VERSITYGW_BIN" --port 127.0.0.1:9000 --access kwaccess --secret kwsecret-0123456789abcdef --region us-east-1 \
  posix ${META[@]+"${META[@]}"} "$(native_path "$DIR/data")" > "$DIR/server.out" 2>&1 &
echo $! > "$DIR/pid"
for _ in $(seq 1 60); do
  if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then break; fi
  sleep 0.5
done
(exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null || { echo "versitygw did not start; see $DIR/server.out" >&2; cat "$DIR/server.out" >&2; exit 1; }

echo "KERNWAY_S3_ENDPOINT=http://127.0.0.1:9000"
echo "KERNWAY_S3_ACCESS_KEY=kwaccess"
echo "KERNWAY_S3_SECRET_KEY=kwsecret-0123456789abcdef"
echo "KERNWAY_S3_REGION=us-east-1"
echo "KERNWAY_S3_SERVER=versitygw $("$VERSITYGW_BIN" --version | tr -d '\r' | sed -n 's/^Version *: *//p')"
