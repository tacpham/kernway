#!/usr/bin/env bash
# Run the three servers one at a time and load each with the same driver.
# Meant to run INSIDE the Linux container, after `cargo build --release`.
#
#   CONC=32 SECS=5 FILE_MIB=32 PAYLOAD_MIB=8 ./run.sh
#
# One server runs at a time (no cross-server CPU contention); each is loaded on
# every mode, then killed before the next starts.
set -u

BIN=target/release
CONC=${CONC:-32}         # concurrent keep-alive connections
SECS=${SECS:-5}          # seconds per mode
FILE_MIB=${FILE_MIB:-32} # size of the /download file
PAYLOAD_MIB=${PAYLOAD_MIB:-8}  # upload / multipart body size

wait_ready() { # $1=port
  for _ in $(seq 1 200); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then exec 3>&- 3<&-; return 0; fi
    sleep 0.05
  done
  return 1
}

bench_one() { # $1=label $2=binary $3=port
  BENCH_FILE_MIB=$FILE_MIB PORT=$3 "$BIN/$2" >"/tmp/$2.log" 2>&1 &
  local pid=$!
  if ! wait_ready "$3"; then
    echo "!! $1 failed to start — see /tmp/$2.log"; cat "/tmp/$2.log"; kill "$pid" 2>/dev/null; return
  fi
  for mode in download upload multipart; do
    "$BIN/load" "127.0.0.1:$3" "$mode" "$CONC" "$SECS" "$PAYLOAD_MIB" \
      | sed "s/^RESULT mode=/$1 /"
  done
  kill "$pid" 2>/dev/null; sleep 1; kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
}

echo "== framework comparison =="
echo "cores=$(nproc) conc=$CONC secs=$SECS download_file=${FILE_MIB}MiB upload/mp_payload=${PAYLOAD_MIB}MiB"
echo
bench_one kernway serve_kernway 8080
bench_one axum    serve_axum    8081
bench_one actix   serve_actix   8082
