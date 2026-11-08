# kernway — Lộ trình hệ cache (17 khái niệm)

> Đối chiếu hệ cache của kernway với 17 khái niệm caching kinh điển, rồi lộ trình nâng cấp. Tài liệu sống. Audit 2026-11-06.

## Hiện trạng
Hệ cache còn non trẻ (~1,556 LOC). `kernway-cache-core` = spec trait; `kernway-cache-memory` = impl duy nhất (HashMap + 1 Mutex toàn cục, TTL lazy); `kernway-redis` = RESP client **chưa** implement trait `Cache`; `kernway-cache-macro` = **stub rỗng** (chưa cache gì).

## Bảng gap (17 khái niệm)
| # | Khái niệm | Trạng thái | Ghi chú |
|---|-----------|-----------|---------|
| 1 | Cache-aside `get_or_load` | ✅ | default method `cache.rs:69` |
| 2 | TTL | ✅ lazy | `cache.rs:27`; không sweep nền |
| 17 | Concurrency | ✅ | 1 Mutex toàn cục `memory/lib.rs:39` |
| 4 | Eviction/LRU | 🟡 | cap có, nhưng evict **tùy tiện** `lib.rs:137` |
| 5 | Key design | 🟡 | typed, thiếu namespace/version/builder |
| 7 | Invalidation | 🟡 | `evict` 1 key + `clear`; thiếu prefix/tag/bulk |
| 15 | Metrics | 🟡 | hit/miss in-mem; thiếu evict/load counter, exporter |
| 3 | TTL jitter | ❌ | — |
| 8 | Single-flight/lease | ❌ | mọi caller miss đều chạy loader |
| 9 | Negative cache | ❌ | không cache not-found |
| 6 | Write-through/back/around | ❌ | chỉ cache-aside |
| 10 | Bloom filter | ❌ | — |
| 11 | Distributed/consistent-hash | ❌ | redis single-node |
| 12 | Hot-key replication | ❌ | — |
| 13 | L1/L2 layering | ❌ | mem & redis rời nhau |
| 14 | Failover/replica | ❌ | reconnect-on-next, không backoff |
| 16 | Macro `#[cacheable]` | ❌ | stub rỗng |

## 2 bài toán lõi → map
- **Bản sao SAI:** TTL+jitter, delete-on-write (idempotent, "delete > update"), key version.
- **Bản sao THIẾU:** single-flight (stampede), negative cache (penetration), jitter (avalanche), warming (cold-start), failover.

## Roadmap (ưu tiên theo ROI cho workload CMS/audiobook, Redis 1 node)

### Tier 1 — rẻ, ROI cao, localized
- ✅ **#8 Single-flight** — XONG (2026-11-06). Primitive `cache-core/src/singleflight.rs` (`SingleFlight<K>`, tái dùng) + override `get_or_load` ở InMemoryCache (+ `peek_hit`). Per-node; distributed cần lease (sau). Test: loader chạy 1 lần dù 8 thread cùng miss; gate không leak; error không cache.
- ✅ **#3 TTL jitter** — XONG (2026-11-06). `InMemoryCache::with_jitter(frac)` + `jittered()` áp ở `put`; PRNG xorshift thread-local (không thêm dep `rand`). Mặc định tắt (non-breaking). Test: ±20% trong [80,120], có spread.
- ✅ **#4 LRU thật** — XONG (2026-11-06). `CacheEntry.last_access` + `touch()` (core); `get`/`peek_hit` touch khi hit; eviction chọn oldest `last_access` (helper `evict_to_cap`, dùng chung cho store + negatives). Test: evict đúng LRU.
- ✅ **#9 Negative caching** — XONG (2026-11-06, **hướng B**). `InMemoryCache::get_or_load_optional(key, ttl, negative_ttl, loader: ->Result<Option<V>>)` — cache cả "not-found" vào negatives map riêng (tombstone `CacheEntry<()>`), bound `NEGATIVE_CAPACITY=10_000` chống flood, single-flighted. Positive store check trước (present shadow negative). Test: loader chạy 1 lần cho key vắng mặt.

### Tier 2 — cấu trúc, vừa
- ☐ **#5 Key namespace + version** — helper build key; bump version khi đổi schema.
- ☐ **#11/#14 Nối kernway-redis vào trait `Cache`** (RedisCache impl) + reconnect backoff → distributed-capable.
- ☐ **#13 L1/L2 tiered** — mem L1 → redis L2.
- ☐ **#15 Metrics** — thêm evict/load counter + exporter.

### Tier 3 — DX / khi cần
- ☐ **#16 Macro `#[cacheable]`** — sinh key + TTL + wiring cache-aside.
- ☐ **#10 Bloom**, **#11 consistent hashing**, **#12 hot-key replication** — chỉ khi scale đòi (đang 1 node Redis).

## KHÔNG làm
- **#6 Write-back** — dễ mất dữ liệu. Dùng **delete-on-write** (idempotent) ở tầng app thay thế (library đã có `evict`).

## Tiến độ
- *(cập nhật khi làm)*
