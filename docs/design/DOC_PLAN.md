# Kernway — Documentation Plan

> Plan for building the official documentation at `kernway.dev/docs`, following the Spring docs model.

---

## Reference model — Spring docs

Spring divides its documentation into 4 clear categories:

| Category | Purpose | Spring example |
|---|---|---|
| **Getting Started** | Get the first app running in < 15 minutes | start.spring.io + quickstart guide |
| **Guides** | Solve one specific use case from start to finish | "Building a REST Service", "Accessing Data with JPA" |
| **Reference** | Complete reference for every feature, config option, and annotation | Spring Framework Reference Documentation |
| **API** | Javadoc generated automatically from source code | docs.spring.io/spring-framework/docs/current/javadoc-api |

Kernway follows these same 4 categories.

---

## Tool — mdBook

```
mdBook: the Rust ecosystem's documentation tool
- The Rust Book uses mdBook
- The Cargo Book uses mdBook
- The Async Book uses mdBook
- Search built in
- Generated from Markdown
- Easy to deploy: GitHub Pages / Netlify
```

```bash
# Setup
cargo install mdbook
mdbook init kernway-docs
mdbook serve   # preview at localhost:3000
mdbook build   # emit HTML into book/
```

---

## Docs site directory structure

```
kernway-docs/                  ← its own repo: github.com/kernway/kernway-docs
├── book.toml                  ← mdBook config
├── src/
│   ├── SUMMARY.md             ← table of contents — the most important file
│   │
│   ├── getting-started/
│   │   ├── README.md          ← Overview
│   │   ├── installation.md    ← installing Rust + kernway-cli
│   │   ├── first-app.md       ← Hello World in 5 minutes
│   │   ├── project-structure.md ← the standard directory layout
│   │   └── for-spring-developers.md     ← Spring developers: read this first
│   │
│   ├── guides/                ← one guide = one complete use case
│   │   ├── README.md
│   │   ├── rest-api.md        ← Building a REST API with CRUD
│   │   ├── database.md        ← DB connection, queries, transactions
│   │   ├── authentication.md  ← JWT auth end to end
│   │   ├── validation.md      ← Request validation, custom validators
│   │   ├── error-handling.md  ← Defining errors, handlers, RFC 7807
│   │   ├── logging.md         ← Log setup, formats, file rotation
│   │   ├── testing.md         ← Unit tests, integration tests, mocks
│   │   ├── hot-reload.md      ← kernway dev + hot reload workflow
│   │   ├── templates.md       ← Server-side rendering with kernleaf
│   │   ├── websocket.md       ← Real-time with WebSocket
│   │   ├── deployment.md      ← Docker, Kubernetes, musl static binary
│   │   └── openapi.md         ← Auto-generated Swagger UI
│   │
│   ├── reference/             ← the complete lookup
│   │   ├── README.md
│   │   ├── annotations.md     ← every annotation, parameter, example
│   │   ├── di-system.md       ← DI, scopes, lifecycle, override
│   │   ├── routing.md         ← route syntax, path params, extractors
│   │   ├── response-types.md  ← Json, Html, Template, redirect...
│   │   ├── error-handling.md  ← AppError, exception_handler priority
│   │   ├── configuration.md   ← config file, env vars, profiles
│   │   ├── logging.md         ← log levels, formats, file config
│   │   ├── security.md        ← CORS, CSRF, rate limit, roles
│   │   ├── database.md        ← DbPool trait, diesel, migrations
│   │   ├── aop.md             ← transactional, cached, retry, circuit_breaker
│   │   ├── testing.md         ← TestApp API, mock beans
│   │   ├── fault-tolerance.md ← 5 error levels, supervisor, graceful shutdown
│   │   ├── hot-reload.md      ← .so plugin, kernway-server, kernway-cli
│   │   ├── standards.md       ← RFC compliance per module
│   │   └── platform.md        ← Linux/macOS/Windows differences
│   │
│   ├── migration/
│   │   ├── README.md
│   │   └── spring-to-kernway.md ← annotation map, pattern map, pitfalls
│   │
│   └── api/
│       └── README.md          ← links to docs.rs/kernway (auto-generated)
│
└── theme/                     ← custom CSS, logo
```

---

## SUMMARY.md — Table of contents (most important)

```markdown
# Summary

[Introduction](README.md)

## Getting Started

- [Installation](getting-started/installation.md)
- [Your First App](getting-started/first-app.md)
- [Project Structure](getting-started/project-structure.md)
- [Coming from Spring](getting-started/for-spring-developers.md)

## Guides

- [Building a REST API](guides/rest-api.md)
- [Database Access](guides/database.md)
- [Authentication & Authorization](guides/authentication.md)
- [Validation](guides/validation.md)
- [Error Handling](guides/error-handling.md)
- [Logging](guides/logging.md)
- [Testing](guides/testing.md)
- [Hot Reload](guides/hot-reload.md)
- [Templates (kernleaf)](guides/templates.md)
- [WebSocket](guides/websocket.md)
- [Deployment](guides/deployment.md)
- [OpenAPI / Swagger](guides/openapi.md)

## Reference

- [Annotations](reference/annotations.md)
- [Dependency Injection](reference/di-system.md)
- [Routing](reference/routing.md)
- [Response Types](reference/response-types.md)
- [Error Handling](reference/error-handling.md)
- [Configuration](reference/configuration.md)
- [Logging](reference/logging.md)
- [Security](reference/security.md)
- [Database](reference/database.md)
- [AOP](reference/aop.md)
- [Testing](reference/testing.md)
- [Fault Tolerance](reference/fault-tolerance.md)
- [Hot Reload](reference/hot-reload.md)
- [Standards Compliance](reference/standards.md)
- [Platform Notes](reference/platform.md)

## Migration

- [Spring Boot → Kernway](migration/spring-to-kernway.md)

## API Reference

- [docs.rs/kernway](api/README.md)
```

---

## Writing priority — by release

### v0.3 (must exist before release)

```
MUST exist:
  getting-started/installation.md
  getting-started/first-app.md
  getting-started/project-structure.md
  getting-started/for-spring-developers.md      ← Kernway's target audience
  guides/rest-api.md
  guides/error-handling.md
  guides/logging.md
  reference/annotations.md
  migration/spring-to-kernway.md
```

### v0.4

```
  guides/database.md
  guides/authentication.md
  guides/validation.md
  guides/testing.md
  reference/di-system.md
  reference/fault-tolerance.md
```

### v0.5+

```
  guides/hot-reload.md
  guides/deployment.md
  guides/templates.md
  guides/websocket.md
  guides/openapi.md
  reference/* (the rest)
```

---

## Writing standard — every page must include

```markdown
# Feature name

> One line stating its purpose.

## Before you start

Prerequisites.

## Quick example

(the smallest snippet that actually runs)

## Detailed explanation

(part by part, option by option)

## Full example

(complete working example)

## Compared with Spring

(when an equivalent feature exists)

## See also

(links to related pages)
```

---

## Docs site config — book.toml

```toml
[book]
title       = "Kernway Documentation"
description = "Rust Web Framework — Spring-inspired"
authors     = ["Kernway Contributors"]
language    = "en"
src         = "src"

[output.html]
site-url           = "https://kernway.dev/docs/"
git-repository-url = "https://github.com/kernway/kernway-docs"
edit-url-template  = "https://github.com/kernway/kernway-docs/edit/main/src/{path}"
theme              = "theme"

[output.html.search]
enable = true
limit-results = 30
teaser-word-count = 30
```

---

## Separate repos

```
github.com/kernway/kernway          ← framework source code
github.com/kernway/kernway-docs     ← documentation site
github.com/kernway/kernway-examples ← example projects

kernway.dev/                        ← landing page
kernway.dev/docs/                   ← mdBook docs
docs.rs/kernway                     ← API reference (generated from rustdoc)
```

---

## CI/CD for docs

```yaml
# .github/workflows/docs.yml
on:
  push:
    branches: [main]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo install mdbook
      - run: mdbook build
      - uses: peaceiris/actions-gh-pages@v3
        with:
          github_token: ${{ secrets.GITHUB_TOKEN }}
          publish_dir: ./book
```

Every push to `main` → automatically build and deploy to GitHub Pages.

---

## Current state — mapping existing docs

The files in the current `docs/` repository will be the **content source** for writing the official docs:

| Current file | Will become |
|---|---|
| `docs/ARCHITECTURE.md` | `reference/di-system.md` + internal dev notes |
| `docs/ANNOTATIONS.md` | `reference/annotations.md` |
| `docs/ROADMAP.md` | Internal — not public on the docs site |
| `docs/STANDARDS.md` | `reference/standards.md` |
| `docs/PLATFORM.md` | `reference/platform.md` |
| `docs/DEVELOPMENT.md` | Internal — separate contributor guide |
| `docs/FEATURES.md` | Split across individual guides |
| `docs/FAULT_TOLERANCE.md` | `reference/fault-tolerance.md` |
| `docs/ERROR_HANDLING.md` | `guides/error-handling.md` + `reference/error-handling.md` |
| `docs/modules/*.md` | Internal — AI implementation notes |
