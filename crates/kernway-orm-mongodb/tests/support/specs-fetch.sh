#!/usr/bin/env bash
# The official MongoDB driver specifications' test data (github.com/mongodb/specifications),
# pinned to one commit and fetched at test time — the data is CC BY-NC-SA 3.0, so it is
# downloaded, not copied into this repository.
#
#   crates/kernway-orm-mongodb/tests/support/specs-fetch.sh [dir]
#
# Prints KERNWAY_MONGO_SPECS=<dir>/source (the spec tree with its tests/ folders).
set -euo pipefail
COMMIT="${MONGO_SPECS_COMMIT:-4597fe9d19640643419df4c216403787a31f81e4}"
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/mongo-specs}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
if [ ! -f "$DIR/$COMMIT/.complete" ]; then
  rm -rf "$DIR/$COMMIT.tmp"
  mkdir -p "$DIR/$COMMIT.tmp"
  curl -fsSL "https://codeload.github.com/mongodb/specifications/tar.gz/$COMMIT" | tar xz -C "$DIR/$COMMIT.tmp" --strip-components 1
  rm -rf "$DIR/$COMMIT"
  mv "$DIR/$COMMIT.tmp" "$DIR/$COMMIT"
  touch "$DIR/$COMMIT/.complete"
fi
echo "KERNWAY_MONGO_SPECS=$DIR/$COMMIT/source"
echo "KERNWAY_MONGO_SPECS_COMMIT=$COMMIT"
