# kernway-dns

> Pure-async DNS stub resolver for the Kernway runtime — no `getaddrinfo`, no
> blocking thread.

`kernway-dns` resolves hostnames over DNS **on Kernway's own async runtime**
(`rt-core` / `rt-net`), so an outbound request never parks a blocking-pool thread
on the OS resolver. It exists because the earlier HTTP client was "async" only at
the TCP layer: name resolution was a synchronous `getaddrinfo` shunted to
`spawn_blocking`, bounded by *no* timeout (`connect_timeout` covered `connect()`,
not `resolve()`). A wedged lookup could tie up a pool thread indefinitely.

This crate replaces that path with a real DNS-over-UDP resolver (with TCP
fallback), leaving `getaddrinfo` only as an optional last-resort fallback behind a
feature flag in `kernway-http-client`.

## Where it fits

```
kernway-http-client::resolve()
  → IP literal / /etc/hosts / localhost      (no lookup)
  → kernway-dns   ── UDP query ─→ kernway-udp::AsyncUdpSocket ─→ rt-core reactor
                  └─ TC=1 ──────→ TCP query  ─→ rt-net::AsyncTcpStream
  → getaddrinfo fallback (feature `getaddrinfo-fallback`, default on)
```

## Installation

Workspace crate — used through `kernway-http-client`. To use it directly:

```toml
[dependencies]
kernway-dns = { path = "crates/kernway-dns" }
rt-core     = { path = "crates/rt-core" }
```

## Usage

```rust,no_run
use kernway_dns::Resolver;
use rt_core::Executor;

let ex = Executor::new().unwrap();
ex.block_on(async {
    // Built from /etc/resolv.conf (falls back to public resolvers if it names none).
    let resolver = Resolver::from_system();

    // A preferred, AAAA fallback; honours search domains and ndots.
    let addrs = resolver.lookup("example.com").await?;

    // Or a specific record type:
    let v4 = resolver.lookup_a("example.com").await?;
    let v6 = resolver.lookup_aaaa("example.com").await?;
    let _ = (addrs, v4, v6);
    Ok::<_, kernway_dns::DnsError>(())
}).unwrap().unwrap();
```

Point it at explicit nameservers (e.g. in tests):

```rust
use std::time::Duration;
use kernway_dns::Resolver;

let resolver = Resolver::new(vec!["1.1.1.1:53".parse().unwrap()])
    .with_timeout(Duration::from_millis(500))
    .with_attempts(2);
```

## Capabilities

| Capability | Status |
|---|---|
| A / AAAA lookups (`lookup_a` / `lookup_aaaa` / `lookup`) | ✅ |
| UDP query with **EDNS0** (advertises a 1232-byte payload) | ✅ |
| **TCP fallback** on the `TC` (truncated) bit | ✅ |
| Per-shard cache (`thread_local`), positive **and** negative TTL | ✅ |
| `/etc/resolv.conf` (nameserver, search, domain, options ndots/timeout/attempts) | ✅ |
| `/etc/hosts` | ✅ (via the caller) |
| Search-domain + `ndots` expansion | ✅ |
| Anti-spoofing: random transaction id + random source port + `connect()` filtering | ✅ |
| Malformed-packet hardening: compression-pointer loop guard, bounds checks | ✅ |

**Scope** — a self-contained stub resolver. Deliberately **not** full
`getaddrinfo` parity: no NSS modules, no mDNS (`.local`), no macOS
System-Configuration / systemd-resolved split-DNS. Those stay behind the
`getaddrinfo` fallback in the HTTP client.

## Performance

The design goals are structural, not nanosecond-level:

- **No blocking thread per lookup** — DNS runs on the shard reactor, so a slow or
  wedged resolver can't exhaust the blocking pool the way `getaddrinfo` could.
- **Lock-free on the hot path** — the cache is per-shard (`thread_local`), chosen
  over a shared `Arc<RwLock>` to keep the thread-per-core resolve path free of
  cross-thread atomics. Trade-off: a popular name may be resolved once per core.
- **Bounded** — every query has an explicit timeout with retry; the fallback is
  wrapped in a 5 s bound so it can't hang the caller.

The wire codec — the CPU-bound part that scales with lookup volume — is
micro-benchmarked (`cargo bench -p kernway-dns`). On the author's machine
(figures are shape, not a cross-machine promise — see
[KEP-0000 §2](../../docs/kep/0000-principles.md)):

| Bench | Time |
|---|---|
| `encode_query` (`www.example.com`, A) | ~39 ns |
| `encode_query_edns` (+ OPT record) | ~40 ns |
| `parse_response` (3× A, compression pointers) | ~352 ns |

The I/O path (UDP/TCP) is reactor-bound and not benchmarked here.

## Testing

45 offline tests — the wire codec (edge cases: compression-pointer loops,
truncation, EDNS), the resolver against a loopback fake server, TCP fallback,
cache TTL, and search/ndots. Plus `#[ignore]` live gates that resolve real names
via a public resolver:

```bash
cargo test -p kernway-dns
cargo test -p kernway-dns --test real_dns -- --ignored   # hits the network
```

## Design & further reading

- Runtime it builds on: [`rt-core`](../rt-core) (reactor) and
  [`rt-net`](../rt-net) (TCP), with [`kernway-udp`](../kernway-udp) for the async
  UDP socket.
- Principles: [KEP-0000](../../docs/kep/0000-principles.md) (responsible
  dependencies, measured performance).

## License

MIT — see the workspace root.
