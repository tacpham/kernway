# Kernway — Standards Compliance

> **Rule #0**: Before implementing any feature, read the corresponding RFC/spec first.

## Mandatory principles

1. **Read the spec first** — each RFC section = one test case
2. **MUST/REQUIRED in an RFC** — no exceptions, no "skip for now"
3. **SHOULD in an RFC** — clearly document if it is not implemented and why
4. **Security specs** — highest priority, OWASP Top 10 = minimum baseline

---

## Compliance Table

| Module | Crate | Standards |
|---|---|---|
| **HTTP/1.1** | `http-proto` | RFC 9110 (semantics), RFC 9112 (message syntax) |
| **HTTP/2** | `http2-proto` | RFC 9113, RFC 7541 (HPACK compression) |
| **URI/URL** | `web-router` | RFC 3986 (URI), RFC 3987 (IRI), WHATWG URL |
| **TLS** | `tls-adapter` | RFC 8446 (TLS 1.3), RFC 6125 (cert validation) |
| **WebSocket** | `ws-proto` | RFC 6455 |
| **Template** | `kernleaf` | WHATWG HTML Living Standard, W3C DOM |
| **Security** | `aop-layer` | OWASP Top 10, OWASP CSRF Cheat Sheet |
| **DI** | `di-core` | JSR-330 patterns (not Java spec, design inspiration) |
| **Validation** | `aop-layer` | RFC 7807 (Problem Details for HTTP APIs) |
| **Metrics** | `kernway-core` | OpenMetrics spec (Prometheus-compatible) |
| **Date/Time** | HTTP headers | RFC 9110 §5.6.7 (HTTP-date format) |
| **Character set** | HTTP bodies | RFC 9110 §8.3.1 (Content-Type + charset) |
| **Cache** | HTTP headers | RFC 9111 (HTTP Caching) |
| **Cookies** | `web-core` | RFC 6265 |
| **Multipart** | `web-core` | RFC 7578 (multipart/form-data) |
| **CORS** | `aop-layer` | Fetch Living Standard (W3C/WHATWG) |
| **SSE** | `web-core` | W3C EventSource / Server-Sent Events |
| **OpenAPI** | `kernway-openapi` | OpenAPI Specification 3.1.0 |

---

## HTTP/1.1 — RFC 9110 + RFC 9112

Sections that must be implemented:

**RFC 9110 (HTTP Semantics)**
- §4.3.1 — GET: safe and idempotent, no request body
- §4.3.3 — POST: not safe, not idempotent
- §4.3.4 — PUT: idempotent
- §4.3.5 — DELETE: idempotent
- §5.1 — Host header: required in HTTP/1.1
- §5.6.2 — Token format
- §7.3.1 — 200 OK, 201 Created, 204 No Content
- §15.5.1 — 400 Bad Request
- §15.5.5 — 404 Not Found
- §15.6.1 — 500 Internal Server Error
- §15.5.4 — 403 Forbidden (not 401 when permission is missing)

**RFC 9112 (HTTP/1.1 Message Syntax)**
- §2.2 — CRLF line endings
- §3 — Request line format
- §4 — Header fields: no duplicate names (except set-cookie), folding is not allowed
- §6 — Transfer-Encoding: chunked
- §7.1 — Persistent connections (Connection: keep-alive)

---

## TLS — RFC 8446

- TLS 1.3 is the minimum (TLS 1.2 optional compatibility mode)
- TLS 1.0, 1.1 — **NOT supported**
- Required cipher suites: `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`
- OCSP stapling (RFC 6960) — required for production
- Certificate transparency (RFC 9162) — log checking

---

## Security — OWASP Top 10

| Risk | Kernway response |
|---|---|
| A01 Broken Access Control | `#[require_role]`, `kw:authorize` template |
| A02 Cryptographic Failures | TLS 1.3 only, rustls (no OpenSSL) |
| A03 Injection | Parameterized queries only (diesel), SQL strings are not supported |
| A05 Security Misconfiguration | Secure defaults: HSTS, CSP, X-Frame-Options headers by default |
| A06 Vulnerable Components | `cargo audit` in CI |
| A07 Auth Failures | Default rate limiting, constant-time comparison for tokens |
| A08 Data Integrity | Request signature, CSRF token |

**XSS Prevention (kernleaf):**
- All `kw:text` values are HTML-escaped by default
- Raw output is only available through explicit `kw:utext` (unescaped text — clearly named)
- CSRF tokens are automatically injected into every form with `method="POST"`

---

## Validation — RFC 7807

Error response format:

```json
{
  "type": "https://kernway.dev/errors/validation",
  "title": "Validation Failed",
  "status": 400,
  "detail": "Request body contains invalid fields",
  "instance": "/users/register",
  "errors": {
    "email": "must be a valid email address",
    "password": "must be at least 8 characters"
  }
}
```

Rust code:
```rust
#[derive(Deserialize, Validate)]
struct RegisterRequest {
    #[validate(email)]
    email: String,
    #[validate(min_length = 8)]
    password: String,
}

#[route(POST, "/users/register")]
#[validated]
async fn register(body: Validated<Json<RegisterRequest>>) -> impl IntoResponse {
    // body.0 is validated — this line is only reached when it is valid
}
```

---

## Dependency Policy

See [DEVELOPMENT.md](DEVELOPMENT.md#dependency-policy) for the full whitelist/blacklist.

Principle: every dependency must be justified by an RFC/spec requirement or platform necessity.
