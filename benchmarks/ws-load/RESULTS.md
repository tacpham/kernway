# ws-load results (dev Mac)

**These numbers come from a laptop, not the production target.** The client and
the server share one machine, the traffic is loopback TCP, and the box was busy
with other applications. Use them to compare builds on this Mac, not as Linux
capacity figures. The Linux procedure is in [README.md](README.md).

## Machine

| | |
|---|---|
| CPU | Apple M2 Max (`sysctl -n machdep.cpu.brand_string`), 12 cores (8 performance + 4 efficiency) |
| Memory | 64 GiB (`hw.memsize` = 68719476736) |
| OS | macOS 26.5 (25F71) |
| Toolchain | rustc 1.96.0, `--release` (workspace profile: opt-level 3, thin LTO) |
| Limits | effective fd limit 245 760 (`kern.maxfilesperproc`), `kern.ipc.somaxconn` 128, ephemeral ports 49152–65535 |
| Background load | load average 6–10 throughout: a Python process at ~90 % CPU, Zalo ~95 %, Chrome, IDE. Expect noise in the tails. |
| Date | 2026-11-24 |

## Setup

`SO_REUSEPORT` does not balance on macOS, so 12 shards on one port would all
share a single core. These runs therefore use **four single-shard apps on four
ports** (4 server cores) and **4 client threads**:

```sh
cargo build --release -p ws-load
target/release/ws-load-server --workers 1 --port 9001,9002,9003,9004 &
T=127.0.0.1:9001,127.0.0.1:9002,127.0.0.1:9003,127.0.0.1:9004
```

Server defaults: `WsConfig::default()` (send_queue 256, ping 25 s, idle 60 s).
Client defaults: `--threads 4 --concurrency 32 --drain-secs 2`, 64-byte binary
messages.

## Headline numbers

| Scenario | Command | Result |
|---|---|---|
| **idle 10k** | `ws-load idle --conns 10000 --hold-secs 10 --connect $T` | 10 000 / 10 000 opened in 0.36 s = **27 978 conn/s**, 0 failed.<br>Connect+upgrade p50 **4.0 ms** / p90 5.8 / p99 15.1 / max 16.1 ms (128 handshakes in flight).<br>Server RSS 2 480 → 55 584 KiB = **5 438 B (5.3 KiB) per connection** (21 858 B before the fix below).<br>Client 3.3 KiB/conn. 0 disconnects in the hold. |
| **echo 1k × 20/s** | `ws-load echo --conns 1000 --rate 20 --duration 30 --connect $T` | 600 000 sent / 600 000 received = **20 000 msg/s**, 0 missing, 0 drops.<br>Round trip (from write) p50 **0.035** / p90 0.056 / p99 **0.099** / p99.9 0.98 / max 7.4 ms.<br>From schedule: p50 0.068 / p99 0.248 / p99.9 1.48 ms.<br>Server CPU 0.30 core, client 0.32 core. Server RSS 8.5 MiB. |
| **fanout 50 × 200 @ 10/s** | `ws-load fanout --rooms 50 --members 200 --rate 10 --duration 30 --connect $T` | 50 groups × 200 members. 15 000 sent (500 msg/s) → 3 000 000 delivered = **100 000 deliveries/s**, 0 missing, 0 drops.<br>Delivery (from write) p50 **1.01** / p90 3.0 / p99 **9.2** / p99.9 55.8 / max 158 ms.<br>Server CPU 0.61 core, client 0.91. Server RSS 59 MiB. |
| fanout, repeated twice (20 s) | same, `--duration 20` | p50 1.09 / 1.01, p99 7.3 / 6.8, p99.9 **22.5 / 20.3**, max 27 / 24 ms. The 56 ms p99.9 of the first run is background noise. |
| echo on 1 port, 12 workers | `ws-load-server --port 9001` + `ws-load echo --conns 1000 --rate 20 --duration 20` | All 1 000 connections landed on shard 11 (macOS). Round trip p50 0.034 / p99 0.095 / p99.9 0.49 ms. Server CPU 0.23. |

What the fanout latency means: a broadcast to 200 members is 200 `writev`
calls on one shard, so the k-th member waits for k writes. The p50 of about
1 ms is that serial cost on macOS loopback, where each `writev` also runs the
receiver's TCP input. Rooms that collide on a shard add to the tail.

## Finding the knee (echo, 1 000 connections, 4 ports)

| rate/conn | offered msg/s | received | round trip p50 / p99 / max (ms) | server CPU | client CPU | notes |
|---|---|---|---|---|---|---|
| 20 | 20 000 | 100 % | 0.035 / 0.099 / 7.4 | 0.30 | 0.32 | |
| 50 | 50 000 | 100 % | 0.045 / 0.225 / 21.8 | 0.84 | 0.87 | |
| 100 | 100 000 | 100 % | 0.754 / 73.9 / 102 | 1.75 | 1.79 | knee |
| 150 | 150 000 | 100 % | 76.5 / 229 / 368 | 0.79 | 1.36 | `kernel_task` ≈ 110 % (baseline 20 %) |
| 200 | 200 000 | 97.5 % in window | 935 / 9 731 / 18 821 | 0.21 | 1.60 | 58 222 server drops (outbox full) |

Fanout at 50 msg/s per room (500 000 deliveries/s offered): 99.8 % delivered.
p50 302 ms, p99 1.8 s, 185 server drops.

**What limits it here is the macOS loopback stack, not kernway-ws.**

- **Neither process is busy.** Past ~100k msg/s, both processes are mostly
  idle. `/usr/bin/sample` shows each server shard about 85 % in `kevent` and
  each client thread about 65 % in `kevent`. Server CPU actually *drops* as
  latency rises, because reads and `writev`s batch more frames per syscall.
- **The kernel is busy.** `kernel_task` climbs by about one full core:
  loopback input is processed by a kernel thread charged to neither process.
- **The client is not the bottleneck.** One client thread instead of four gives
  the same latency at 150k msg/s (p50 62 ms).
- **The server's backpressure works as designed.** Under overload the
  outboxes fill up, and `SendError::Full` is counted as a drop instead of
  memory growing.

This ceiling has to be re-measured on Linux with the client on another box.

## Bug found and fixed: upgraded connections pinned 16 KiB of HTTP buffers

**Symptom.** The idle 10k run measured **21.35 KiB of server RSS per idle
WebSocket**. `kernway-ws` says an idle connection costs about 1 KiB of WS
state.

**Cause.** In `crates/kernway-server/src/app.rs`, `serve_connection` (the
`Body::Upgrade` branch, fixed at lines 822–827) awaited `on_upgrade(...)` while its two
pooled HTTP read buffers (`buf`, `chunk`: 2 × `READ_CHUNK` = 16 KiB) were still
in scope. Every upgraded connection therefore held them for its whole lifetime
instead of returning them to the shard's pool.

**Fix.** Two lines: `drop(buf); drop(chunk);` right after the pipelined bytes
are copied into `Upgraded::buffered`, before the hand-over.

**Test.** `app::keep_alive_tests::upgrade_returns_the_http_read_buffers` adds a
`/upgrade-pool` route whose upgrade task reports the shard pool's free-buffer
count. It failed before the fix (`pool=0`) and passes after (`pool=2`). It
needs a `#[cfg(test)] bufpool::free_count()` helper in
`crates/kernway-server/src/bufpool.rs`.

| idle 10k | server RSS after connect | per connection |
|---|---|---|
| before | 215 968 KiB | 21 858 B (21.35 KiB) |
| after | 55 584 KiB | 5 438 B (5.31 KiB) |

## Not fixed: owner's call

The suspended `serve_connection` future stays allocated for the connection's
lifetime and costs about **2.3 KiB per WebSocket**.

**Experiment.** I temporarily replaced `on_upgrade(...).await; return;` with
`rt_core::spawn(on_upgrade(...)); return;` and measured idle 10k: **3 100 B
(3.03 KiB) per connection, down from 5.3 KiB**. I reverted it afterwards.

**Why it is not in this change:**

- It changes task structure: the connection task ends at the hand-over.
- It changes test semantics: the `with_server` test helper `block_on`s
  `serve_connection` directly and would no longer drive the upgrade.

It is listed here for the owner to decide. At 100k connections it is about
230 MB.

## Caveats

- **Shared machine.** Client and server share 12 cores and the loopback stack.
  The CPU figures are `ps` / `getrusage` deltas over the send window, so the
  client's load is visible, but contention is not separated out.
- **Single-shard apps.** "4 shards" here means four single-shard apps on four
  ports. Production uses one app whose shards are balanced by `SO_REUSEPORT`.
- **RSS method.** RSS is read with `ps` (macOS `ps` reports KiB). It includes
  allocator retention and the shard buffer pools. "Per connection" is the delta
  divided by connections opened, sampled 2 s after the last connect.
- **Timer slack.** The "from schedule" latency includes the client's macOS
  kevent timer slack (≈ 0.03–0.5 ms at p50). "From write" excludes it.
- **The 16k wall.** The idle run was not pushed past 10k. macOS allocates
  ephemeral ports from one 16 384-port range per *source IP*, whatever the
  destination port: an idle 30k attempt over 4 ports stopped at 16 351
  connections with `EADDRNOTAVAIL`. Going further needs `--bind` with lo0
  aliases (sudo), which this run did not use.
