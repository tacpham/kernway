# ws-load: load tests for kernway-ws

This crate is a load-test harness for the `kernway-ws` WebSocket server. It is a
workspace member with two binaries:

| binary | what it is |
|---|---|
| `ws-load-server` | A `KernwayApp` serving `GET /echo`, `GET /room/{id}` and `GET /stats`. |
| `ws-load` | The load generator. It runs on rt-core/rt-net (dogfooding): N client threads, each with one `Executor` driving thousands of connections through a minimal RFC 6455 client (masked frames, a fresh random mask per frame). |

Measured numbers from a dev Mac are in [RESULTS.md](RESULTS.md).

```sh
cargo build --release -p ws-load
target/release/ws-load-server --port 9001 &
target/release/ws-load echo --conns 1000 --rate 20 --duration 30
```

## Server (`ws-load-server`)

```
ws-load-server [--host 127.0.0.1] [--port 9001]... [--workers N]
               [--send-queue 256] [--idle-timeout-secs 60]
```

- **`/echo`** sends every message back to its sender. If the outbox is full
  (`SendError::Full`), the server counts the message as a drop.
- **`/room/{id}`** sends every message a member posts to every member of that
  room *on the same shard*, sender included. The message is encoded once with
  `PreparedMessage` and fanned out by pointer. A receiver whose outbox is full
  misses that message and the server counts the drop. Each member's first frame
  is a text `hello pid=… slot=… shard=… room=…`, which the client uses to work
  out the shard-local group the member is in.
  Rooms are a thread-local `HashMap<room, Vec<WsSender>>` per shard. **Rooms that
  span shards are out of scope here.** The game service routes those through
  `rt_net::CoreMesh`. On Linux, `SO_REUSEPORT` hashes a room's members across
  shards, so a room becomes one group per shard, and the client gives each
  group its own sender (see fanout below).
- **`/stats`** returns JSON counters. Each shard has its own cache-padded slot,
  written only by that shard thread, so counting adds no contention:
  `pid, conns_open, conns_total, msgs_in, msgs_out, drops, send_closed`, plus a
  `shards[]` array with the same fields per `(port, shard)`.
- `--port` can be repeated or given a comma-separated list. Each port gets its
  own `KernwayApp` with `--workers` shards, all in one process.
- `--send-queue` is `WsConfig::send_queue`, the per-connection outbox limit.
- At startup, both binaries raise `RLIMIT_NOFILE` as far as allowed and print
  the effective limit. On macOS that limit is capped by `kern.maxfilesperproc`.

## Client (`ws-load`)

```
ws-load idle   --conns N [--hold-secs 10]
ws-load echo   --conns N --rate R --duration S [--size 64]
ws-load fanout --rooms K --members M --rate R --duration S [--size 64]

common:  --connect HOST:PORT[,..]   targets, round-robin per connection
                                    (fanout: a whole room goes to one target)
         --bind SRC_IPV4[,..]       source addresses, round-robin
         --threads 4                client shards (threads)
         --concurrency 32           handshakes in flight per thread
         --server-pid PID           default: the pid in /stats
         --stats HOST:PORT          default: every --connect target
         --drain-secs 2             wait for in-flight messages after the window
```

### What each scenario reports

- **Every scenario: the connect phase.** It reports connections opened and
  failed, the error kinds, the rate (opened connections divided by the
  wall-clock time of the phase), and connect latency percentiles. Connect
  latency runs from just before TCP `connect` to the `101` response (and, for
  fanout, the room's hello). The report also shows how connections landed per
  server `port:shard`.
- **idle**
  - **Server RSS:** the client reads it with `ps -o rss= -p <pid>` before
    connecting, 2 s after the last connection, and at the end of `--hold-secs`.
    It prints `(connected − before) / opened` as **bytes per connection**.
  - **Server process:** the server runs as a separate process, as it would in
    production. Its pid comes from `/stats`, or from `--server-pid`, for
    example when the server runs on another box.
  - **Client RSS** per connection and **unexpected disconnects** during the hold
    are also reported.
- **echo**
  - **Load:** every connection sends `--rate` messages/s (per connection) for
    `--duration` seconds.
  - **Results:** achieved send rate and receive rate, missing messages
    (expected − received after the drain), and round-trip latency
    percentiles (p50/p90/p99/p99.9/max).
  - **Server side:** `msgs_in`/`msgs_out`/`drops` deltas from `/stats`, and
    server CPU (`ps -o time=` delta) during the window.
  - **Client side:** client CPU (`getrusage`), so you can see whether the client
    was the bottleneck.
- **fanout**
  - **Load:** K rooms of M members. One sender per room group (room × server
    shard) sends `--rate` messages/s.
  - **Latency** is delivery latency measured at every member.
  - **Throughput:** deliveries/s, with expected deliveries = Σ sent × group
    size, plus server drops.

### How latency is measured

- **Clock.** Each timed message carries two little-endian `u64` stamps from one
  process-wide monotonic clock, so a stamp written on one client thread can be
  read on another.
  - **`scheduled`** is when the open-loop pacer meant to send the message.
    Message *k* is due at `first + k/R` whatever happened to message *k−1*. The
    first send is offset by a random phase per connection so that senders are
    not in lockstep.
  - **`sent`** is when it was actually handed to the connection's writer.
- **"from write"** is `now − sent`: the service latency through the network
  and the server. This is the headline number.
- **"from schedule"** is `now − scheduled`. It is corrected for coordinated
  omission, so it also includes any lateness of the client's own timer. On
  macOS that includes kevent timer slack (~0.05–0.5 ms). On Linux, `epoll_wait`
  timeouts are whole milliseconds, so expect up to 1 ms of client timer slack
  in this column.
- **Histogram.** The histogram is in `src/hist.rs`, written here rather than
  pulled in as a dependency. It is log-linear (HdrHistogram-style), with 128
  sub-buckets per power of two, so the relative error is at most 0.78 %. A
  percentile reports the bucket's **upper** edge, which means it is never
  better than the truth. `max` and `mean` are exact.
- **Counting.** The client counts messages received during the window plus
  `--drain-secs` after it. Anything later is reported as missing.

## macOS notes (dev machine)

- **`SO_REUSEPORT` does not balance on macOS.** Every connection lands on *one*
  shard, so `--workers 12 --port 9001` runs on a single core. To use several
  cores, give each port its own single-shard app and spread clients over the
  ports:
  ```sh
  ws-load-server --workers 1 --port 9001,9002,9003,9004
  ws-load echo ... --connect 127.0.0.1:9001,127.0.0.1:9002,127.0.0.1:9003,127.0.0.1:9004
  ```
- **The ephemeral port range is global per source IP.** On macOS that is
  16 384 ports by default (49152–65535), *whatever the destination port*.
  Several server ports do **not** get you past about 16k connections (tested:
  the 16 352nd connect fails with `EADDRNOTAVAIL`). Either option below works:
  - Add loopback aliases and bind sources:
    ```sh
    sudo ifconfig lo0 alias 127.0.0.2 up
    sudo ifconfig lo0 alias 127.0.0.3 up
    ws-load idle --conns 40000 --bind 127.0.0.1,127.0.0.2,127.0.0.3
    ```
  - Widen the range: `sudo sysctl -w net.inet.ip.portrange.first=10000`.
- **The fd ceiling** is `kern.maxfilesperproc` (245 760 here). Raise it with
  `sudo sysctl -w kern.maxfiles=... kern.maxfilesperproc=...`.
- **Loopback is a ceiling.** Loopback TCP is processed by the kernel. At around
  100k small messages/s (echo, in plus out), `kernel_task` saturates about one
  core while both processes sit mostly idle in `kevent`, and latency grows into
  the hundreds of ms. That is the Mac's limit, not the framework's. See
  RESULTS.md.
- **`kern.ipc.somaxconn` is 128.** Keep `--threads × --concurrency` around 128
  so SYNs are not dropped into a 1 s retransmit.

## Linux: the 50–100k connection test

Run the server and the client on **separate machines**: on one box they share
CPU, memory bandwidth and the kernel's network stack, and the client's numbers
contaminate the server's. A 100k test needs nothing exotic, but the defaults
are too small.

### Both machines

```sh
# file descriptors (system-wide, per process)
sudo sysctl -w fs.file-max=2097152 fs.nr_open=2097152
ulimit -n 1048576          # or LimitNOFILE=1048576 in the systemd unit,
                           # or "* - nofile 1048576" in /etc/security/limits.conf
# accept queues / SYN backlog (KernwayApp listens with backlog 1024 per shard)
sudo sysctl -w net.core.somaxconn=65535 net.ipv4.tcp_max_syn_backlog=65535
sudo sysctl -w net.core.netdev_max_backlog=65536
# if conntrack is loaded (iptables/nftables NAT, docker), it caps connections too
sudo sysctl -w net.netfilter.nf_conntrack_max=1048576 2>/dev/null || true
```

### Client machine

- **Ports.** Use the widest port range and allow TIME_WAIT reuse for reconnect
  churn:
  ```sh
  sudo sysctl -w net.ipv4.ip_local_port_range="1024 65535"   # ~64k per (src IP, dst IP:port)
  sudo sysctl -w net.ipv4.tcp_tw_reuse=1
  ```
- **Getting past one tuple.** Linux keeps ephemeral ports unique per 4-tuple,
  so the 64k limit is per (source IP, destination IP:port). For 100k, use
  either of these:
  - **Two or more source IPs (preferred):** one server app, so the shard
    layout matches production.
    ```sh
    sudo ip addr add 10.0.0.12/24 dev eth0      # extra source address(es)
    ws-load idle --conns 100000 --connect 10.0.0.1:9001 --bind 10.0.0.11,10.0.0.12 \
                 --threads 8 --concurrency 64
    ```
    The harness sets `IP_BIND_ADDRESS_NO_PORT` on bound sockets, so ports are
    still chosen per destination.
  - **Two or more server ports:** `ws-load-server --port 9001,9002`, then
    `--connect 10.0.0.1:9001,10.0.0.1:9002`. This doubles the shard threads (one
    app per port), so set `--workers` to half the cores.
- **Client sizing.** Give the client enough threads that its CPU stays well
  under `--threads` cores. ws-load prints its own CPU use, and if it nears
  `--threads` the client is the bottleneck. On Linux, one client thread
  comfortably drives 25–50k idle connections.
- **Kernel memory.** About 100k idle sockets cost a few hundred MB of kernel
  memory. If the box is small, lower the default buffer sizes (the third value
  is the autotuning max):
  ```sh
  sudo sysctl -w net.ipv4.tcp_rmem="4096 16384 4194304" net.ipv4.tcp_wmem="4096 16384 4194304"
  ```

### Server machine

```sh
ws-load-server --host 0.0.0.0 --port 9001 --workers $(nproc)   # SO_REUSEPORT balances here
```

- **CPU pinning.** Shards pin themselves to cores (`ShardConfig::pin_threads`),
  so keep other load off the box.
- **`/stats`.** `shards[]` shows the per-shard spread. Expect it to be roughly
  even on Linux.

### Suggested sequence (each ≤ 60 s)

```sh
ws-load idle   --conns 100000 --hold-secs 30 --connect S:9001 --bind A,B --threads 8 --concurrency 64
ws-load echo   --conns 50000  --rate 2  --duration 30 --size 64  --connect S:9001 --bind A,B --threads 8
ws-load fanout --rooms 500 --members 200 --rate 10 --duration 30 --connect S:9001 --bind A,B --threads 8
```

Check these in the output:

- `failed 0`
- `unexpected disconnects 0`
- `missing` close to 0
- `drops(outbox full)` close to 0, unless the run is meant to overload
- client CPU well under `--threads`

Record `uname -a`, `nproc`, memory, the sysctls above, and both machines' load.
