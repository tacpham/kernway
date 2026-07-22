# web-router — Radix Tree Router

## Purpose

Route HTTP requests to handlers via a radix tree. Zero allocation on match path.

## Standards

- **RFC 3986** — URI syntax, path segments, percent-encoding
- **RFC 9110 §9** — Method semantics (safe, idempotent)

## Route Registration

```rust
let mut router = Router::new();
router.add(Method::GET,    "/users",          Box::new(list_users));
router.add(Method::POST,   "/users",          Box::new(create_user));
router.add(Method::GET,    "/users/{id}",     Box::new(get_user));
router.add(Method::PUT,    "/users/{id}",     Box::new(update_user));
router.add(Method::DELETE, "/users/{id}",     Box::new(delete_user));
router.add(Method::GET,    "/users/{id}/posts/{post_id}", Box::new(get_post));
```

## Radix Tree Structure

```
GET /
├── users
│   ├── [exact] → list_users / create_user
│   └── {id}
│       ├── [exact] → get_user / update_user / delete_user
│       └── posts/{post_id} → get_post
└── health → health_check
```

## Path Parameter Extraction

```rust
// Route: /users/{id}/posts/{post_id}
// URI:   /users/42/posts/7

// Extracted automatically into request extensions:
let id: u64 = Path::extract(&req, "id")?;           // 42
let post_id: u64 = Path::extract(&req, "post_id")?; // 7
```

## Percent-Encoding (RFC 3986 §2.1)

- Path matching: decoded before match
- Path params: percent-decoded automatically
- Query string: decoded per key-value pair

## Match Priority

1. Exact match (`/users/me`) beats param match (`/users/{id}`)
2. More specific path wins (`/users/{id}/posts` beats `/users/{id}`)
3. 405 Method Not Allowed (not 404) when the path exists but the method doesn't match

## Wildcard Routes

```rust
// Serve static files under /assets/**
router.add(Method::GET, "/assets/{*path}", Box::new(static_file_handler));
```
