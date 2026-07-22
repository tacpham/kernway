# Deployment

> Coming in v0.5

## Docker

```dockerfile
FROM rust:1.76 AS builder
WORKDIR /app
COPY . .
RUN cargo install kernway-cli && kernway build --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/my-app /app
EXPOSE 8080
CMD ["/app"]
```

> Final image: ~5-10MB
