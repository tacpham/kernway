# kernway — Lộ trình tối ưu hiệu năng

> Cảm hứng từ thử thách "social API trên server 1 vCPU / 2GB" (io_uring, batching, buffer reuse, tách WAL khỏi loop). Tài liệu sống: cập nhật khi làm xong từng mục. Mọi file:line theo bản khảo sát hot-path 2026-11-05.

## Nguyên tắc dẫn đường
Trên **1 vCPU** kernway chạy **đúng 1 shard** → sharding/multi-core không giúp; **hiệu quả của một luồng đơn** (số alloc & syscall mỗi request) là tất cả. Server kernway **không có TLS inbound** (plaintext, TLS ở proxy ngoài) → bottleneck nằm ở allocation per-request + syscall, không phải crypto.

## Đã làm đúng sẵn (không cần đụng)
- **SQLite offload** sang blocking pool (`kernway-orm-sqlite/src/repository.rs:544` `spawn_blocking`), WAL set lúc mở (`driver.rs:22,32`), **không checkpoint/fsync trên shard** → nguyên lý "tách WAL khỏi loop" coi như có sẵn.
- **Sharded SO_REUSEPORT**, 1 shard/CPU (`rt-net/src/shard.rs:54`), accept drain tới `WouldBlock` (đúng edge-triggered), backlog 1024.
- **Headers parse 1-buffer**, borrow slice, không alloc mỗi header (`kernway-http/src/parser.rs:145`, `kernway-core/src/fields.rs:47`).
- `nodelay` bật per-connection (`shard.rs:226`); status/content-length dùng `itoa` stack, không `format!` (`writer.rs:120`).

## Roadmap (ưu tiên theo ROI / rủi ro)

### ✅ 1. writev cho response in-memory — XONG (2026-11-05)
Trước: `encode_response_with` copy **cả body** vào buffer gộp head+body rồi `write_all`. Giờ: `AsyncTcpStream::write_head_body` gửi `[head, body]` bằng **một `writev`** — body không bị copy, vẫn một syscall (giữ tính chống-Nagle).
- `rt-net/src/stream.rs` — thêm `write_head_body` (writev 2 iovec, advance qua partial write).
- `kernway-server/src/app.rs:940` — arm `Body::Empty | Body::Bytes(_)` dùng `encode_head` + `write_head_body`.
- Lợi: bỏ 1 copy body + giảm buffer gộp xuống chỉ còn head (~100B). Rõ rệt với body lớn (trang render, JSON lớn).
- Verify: build sạch, 100/100 test kernway-server + rt-net pass (gồm partial-write payload lớn).

### ✅ 2. Buffer pool tái dùng xuyên connection (per-shard, no-lock) — XONG (2026-11-05)
Trước: mỗi connection `Vec::with_capacity(8K)` + `vec![0u8;8K]`, free khi đóng → churn allocator. Giờ: **thread-local free-list per-shard** (shared-nothing → không khóa) + RAII guard trả buffer khi drop (an toàn mọi lối thoát/panic).
- `kernway-server/src/bufpool.rs` (mới) — `PooledBuf::cleared`/`filled`, deref tới `Vec<u8>` (drop-in). Bound: MAX_POOLED=512 buffer/shard, drop buffer > 64KiB.
- `app.rs:638` — `buf`/`chunk` lấy từ pool.
- Verify: build sạch, 103/103 test (gồm test reuse đúng allocation + oversized không pool).
- Nối tiếp #3: dùng pool này cho cả buffer serialize response (JSON + head).

### ☐ 3. Tái dùng buffer serialize JSON + head
`kernway-web/src/lib.rs:41` `serde_json::to_vec` tạo Vec mới mỗi response; `encode_head` cũng alloc head mỗi response. Serialize body + build head vào **buffer pooled** (từ #2) → cùng writev (#1) đạt gần zero-alloc cho response path.

### ☐ 4. Method thành enum thay String
`parser.rs:222` `method.to_ascii_uppercase()` alloc 1 String/request cho tập đóng (GET/POST/PUT/DELETE/HEAD/PATCH/OPTIONS). Map sang enum + `&'static str` → hết alloc, so sánh nhanh hơn.

### ☐ 5. Bỏ copy body request (`.to_vec()`)
`app.rs:718` copy body ra khỏi read-buffer mỗi request ghi. Cho `Request` mượn slice từ read-buffer (lifetime) hoặc dùng `Bytes`-like refcount → bỏ copy. Refactor lifetime lớn hơn — làm sau #2–#4.

### ☐ 6. PGO cho release binary
Giống bản Crystal trong bài. Thêm profile build (`-Cprofile-generate` → chạy benchmark → `-Cprofile-use`) vào script release. Build-level, không đụng logic.

### ☐ 7. Soi batching event loop
`reactor.rs` drain hết 1024 event/`poll`; `executor.rs:96` `IO_POLL_INTERVAL=100µs` ration poll. Đo xem dưới tải cao có bỏ đói I/O hay gom batch chưa tối ưu không; cân nhắc cap số completion/round để bảo vệ P99 (bài học từ bản Assembly bị kẹt 13s).

### ☐ 8. (LỚN) io_uring backend — feature-gated reactor
Thay/bổ sung epoll bằng io_uring ở `rt-core`/`rt-net`: submission/completion theo batch, **registered buffers**, **direct descriptors** (vượt trần 65k fd). Trần cao nhất nhưng là **viết lại lớn, Linux-only**. Làm sau cùng, sau khi #2–#5 đã vắt kiệt đường epoll. Giữ epoll làm fallback (macOS dev + non-Linux).
- Tham khảo: tất cả top-10 của thử thách hội tụ về io_uring + direct descriptors + batching.

## KHÔNG làm (mẹo riêng của benchmark, hại cho production)
Tự viết storage engine đọc file SQLite; chọc thẳng B-Tree cursor nội bộ; SIMD verify token. Rủi ro mất dữ liệu + bảo trì — chính tác giả cuộc thi cũng cảnh báo.

## Cách đo
Mỗi mục: benchmark trước/sau trên cùng tải (xem `benchmarks/`). Ghi RPS, P95/P99, RSS. Không merge nếu P99 xấu đi.
