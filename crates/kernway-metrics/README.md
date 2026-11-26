# kernway-metrics

Prometheus-compatible metrics for Kernway's thread-per-core runtime. Recording
a metric is one uncontended `Relaxed` atomic add into the calling shard's own
cache line; all the summing happens on scrape.

```rust
use kernway_metrics::{global, HistogramConfig};

// Startup (cold path): register once, keep the handles.
let reg = global(); // or an explicit `Registry::new()`
let responses = reg.counter_vec("http_responses_total", "Responses sent.", &["code"])?;
let ok = responses.with(&["200"]);                     // label lookup happens here, once
let in_flight = reg.gauge("http_in_flight", "Requests being served.")?;
let latency = reg.histogram(
    "http_request_duration_seconds",
    "Request latency.",
    HistogramConfig::latency_seconds(),                // records ns, exports seconds
)?;

// Hot path, on any shard:
in_flight.inc();
let timer = latency.start_timer();
/* ... handle the request ... */
drop(timer);                                           // records elapsed ns
ok.inc();
in_flight.dec();
```

Serving it:

```rust
App::new()
    .get("/metrics", |_req: Request, _scope: &RequestScope| async {
        kernway_metrics::response(kernway_metrics::global())
    })
```

`response` returns a `kernway_core::response::Response` (`200`,
`text/plain; version=0.0.4; charset=utf-8`); `exposition(&registry)` returns
the body as a `String`. The crate depends on `kernway-core` and `rt-net`, not
on `kernway-server`.

## API

| Item | What |
|---|---|
| `Registry::new()` / `with_shard_slots(n)` / `.with_process_metrics(true)` | A set of families. Default: one slot per CPU, no process metrics. |
| `global()` | Process-wide `&'static Registry` (process metrics on). |
| `reg.counter / gauge / histogram(name, help[, config])` | Unlabelled metric → handle. |
| `reg.counter_vec / gauge_vec / histogram_vec(name, help, &labels[, config])` | Labelled family; `.with(&values)` / `.try_with(&values)` → handle. |
| `Counter` | `inc`, `add(u64)`, `get` |
| `Gauge` | `inc`, `dec`, `add(i64)`, `sub`, `set` (per-shard, see below), `get` |
| `Histogram` | `record(u64)`, `record_duration`, `start_timer() -> Timer`, `count`, `sum`, `quantile(q)` |
| `HistogramConfig` | `new(&bounds)`, `.with_scale(f)`, `latency_seconds()`, `bytes()`; `exponential_buckets(start, factor, n)` |
| `exposition(&reg) -> String`, `response(&reg) -> Response`, `CONTENT_TYPE` | Text format 0.0.4. |
| `Counter::detached(n)` etc. | Unregistered metrics for tests/benches. |
| `MetricsError` | `InvalidName`, `InvalidLabelName`, `Conflict`, `LabelCount`, `InvalidBuckets`. |

## Design

### Per-shard slots, no shared writes

Every metric owns `shard_slots + 1` slots. A thread's slot is
`rt_net::shard_index()` — set once when a shard thread starts — and every other
thread (main, blocking pool, a shard index beyond `shard_slots`, a second
server in the same process) uses the last, shared **overflow** slot. Since
connections never migrate between cores, a request's metrics are written by
exactly one core, into a cache line no other core writes. The scrape thread
reads and sums all slots.

- **Writes are `fetch_add(Relaxed)`**, not a load+store "owner-only" update.
  An uncontended atomic add on the owning core is as cheap as a plain add
  (AArch64 LSE `ldadd`, x86 `lock xadd` on an exclusively-owned line), and it
  stays correct when several threads share a slot (the overflow slot, or two
  runtimes both numbering shards from 0) — a non-atomic owner-only update
  would lose increments there. No CAS loops, no locks.
- **Relaxed is enough**: a metric is a statistic, not a synchronisation
  point; a scrape may observe slots at slightly different instants, and the
  sum is somewhere between the totals at the start and end of the scrape.
- **Slots are `#[repr(align(128))]`.** Apple Silicon has 128-byte cache
  lines; on x86 the L2 spatial ("adjacent line") prefetcher fetches 64-byte
  lines in 128-byte pairs, so 64-byte padding still lets neighbouring slots
  interfere. 128 is what `crossbeam_utils::CachePadded` uses on
  x86_64/aarch64. Cost: 128 B × (shards + 1) per counter/gauge — 1.6 KiB on a
  12-core box.
- **Slot count** defaults to `available_parallelism()`, which is also
  `rt_net::ShardConfig`'s default shard count. Running more shards than slots
  is correct but those shards share the overflow slot — size with
  `Registry::with_shard_slots`.

### Handles, labels resolved at registration

`Counter`, `Gauge`, `Histogram` are one `Arc` each. `CounterVec::with(&["200"])`
looks up/creates the series under a lock — call it at startup and keep the
handle. Nothing on the record path hashes, allocates or locks. Label strings
are escaped and joined once, at series creation.

### Histogram: log-linear fine buckets

Values are `u64` in a base unit (ns, bytes). The fine bucket index is
branch-free:

```text
exp   = 63 - leading_zeros(v | 16)      // floor the exponent at 4
shift = exp - 4
index = (shift << 4) + (v >> shift)
```

Values `0..32` are exact; above that each power of two is split into 16
sub-buckets (4 significant bits), so every bucket is at most **6.25 %** wide
relative to its lower bound and a midpoint estimate is within **~3.1 %** of
any value in it, across the full `u64` range — 976 buckets, 7.8 KiB per shard
per series. Three significant bits would halve that but give 12.5 % buckets,
too coarse to read p99s from. Shard bucket arrays are allocated lazily on a
shard's first record (one `OnceLock` load on the hot path), so idle shards cost
8 bytes, not 7.8 KiB.

`record` is **two** `Relaxed` adds — bucket and sum. There is no count
field: `_count` is computed on scrape as the sum of the buckets, which saves an
add and keeps `_count` exactly equal to the `+Inf` bucket.

**Export.** `HistogramConfig` holds the exported `le` bounds (in exported
units) and a scale (`latency_seconds()` records ns and exports seconds with
`scale = 1e-9` and Prometheus's default `.005 … 10` bounds). A fine-bucket →
`le` map is precomputed per family; each fine bucket goes to the first `le` ≥
its *highest* value. A fine bucket straddling a bound is therefore counted
under the next bound: an exported bucket can under-count values within 6.25 %
below its bound, never over-count. Powers of two in base units are exact
bounds.

`quantile(q)` returns the midpoint of the fine bucket holding rank
`ceil(q·count)` — for tests and dev dashboards; Prometheus computes quantiles
server-side from `_bucket`.

### Gauges in a sharded world

A gauge's value is the **sum of its slots**. `inc`/`dec`/`add` compose exactly
(in-flight requests, open connections). `set(v)` sets *the calling shard's
contribution*: each shard `set`-ing its own local quantity (queue depth,
cache size) exports the process total, and a single non-shard owner `set`-ing
from the overflow slot is a plain global set. Mixing `set` from one thread with
`add` from shards is not meaningful.

### Registry and duplicates

Families are keyed by name (`BTreeMap`), series by label values (`BTreeMap`),
so exposition order is deterministic. Registering an existing name with the
**same kind, help, label names and histogram config returns the existing
metric** (same storage — safe to call from every shard's `on_shard_start`);
any mismatch is `MetricsError::Conflict`. Names and label names are validated
(`[a-zA-Z_:][a-zA-Z0-9_:]*`, `[a-zA-Z_][a-zA-Z0-9_]*`, no `__` prefix, no
duplicate labels, no `le` on histograms). A `*_vec` family with no resolved
series is not exported.

### Exposition

Text format 0.0.4: `# HELP` (escaping `\` and newline), `# TYPE`, samples;
label values escape `\`, `"`, newline; histograms emit cumulative
`_bucket{…,le="…"}` including `le="+Inf"`, then `_sum` and `_count`. With
process metrics on, `process_cpu_seconds_total`, `process_max_fds`,
`process_open_fds` and `process_resident_memory_bytes` follow, read at scrape
time (macOS: `proc_pidinfo` `PROC_PIDTASKINFO` / `PROC_PIDLISTFDS`; Linux:
`/proc/self/statm`, `/proc/self/fd`; both: `getrusage`, `getrlimit`). A value
the platform cannot provide is omitted, not reported as 0.

### Timer cost

`start_timer()` + drop is two `Instant::now()` plus a `record`. On macOS
`Instant::now()` measured ~18 ns here, so a timer is ~50 ns — noise for a
request, too much for a tight inner loop (sample there instead).

## Benchmarks

`cargo bench -p kernway-metrics --bench hot_path`. The concurrent group runs on
**real rt-net shard threads** (started by `run_shards_with_init`, pinned when
the OS allows), releases N of them through a barrier, has each perform the op
`iters` times and takes the slowest shard's time — so the figure is "one op on
each of N cores at once". Flat across N = no contention.

Apple M2 Max (8P + 4E cores), Rust 1.96, `--warm-up-time 1 --measurement-time 2`:

| ns / op (each of N shards concurrently) | 1 shard | 2 shards | 4 shards | 8 shards |
|---|---:|---:|---:|---:|
| `counter.inc` | 3.0 | 2.0 | 2.0 | 2.4 |
| `histogram.record` | 2.4 | 2.4 | 2.5 | 3.7 |
| shared `AtomicU64::fetch_add` (naive baseline) | 2.0 | 19.0 | 67.5 | 256 |

| single thread | |
|---|---:|
| `counter.inc` off-shard (overflow slot) | 2.0 ns |
| `Instant::now()` | 18.2 ns |
| `Timer` (start + drop) | 49.2 ns |
| `exposition` — 100 counters + 10 histograms, 9 slots | 35.5 µs |

The per-shard counter stays at ~2 ns from 1 to 8 cores; the shared atomic
degrades ~128× as every core fights for one cache line. The small rise at 8
shards is the slowest-of-8 measurement landing on an efficiency core (the M2
Max has only 8 performance cores). These are short runs — treat ±10 % as noise.

## Not done / possible next steps

- **Per-shard arenas.** Each metric's slots are padded separately, so a shard
  touching ten counters touches ten cache lines. Packing all of one shard's
  metrics into a contiguous per-shard block (indexed by metric id) would make
  that one or two lines, at the cost of a more complex registry.
- Windows and the BSDs report no process metrics yet (TODO in `process.rs`;
  Windows needs a `windows-sys` dependency the workspace does not declare).
- No summaries, exemplars, or OpenMetrics format; no callback ("collector")
  gauges evaluated at scrape time.
