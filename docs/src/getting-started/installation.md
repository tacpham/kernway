# Installation

## Requirements

- Rust 1.75+ ([rustup.rs](https://rustup.rs))
- Cargo (included with Rust)

## Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Verify:

```bash
rustc --version   # rustc 1.75.0 (...)
cargo --version   # cargo 1.75.0 (...)
```

## Install kernway-cli

```bash
cargo install kernway-cli
```

Verify:

```bash
kernway --version   # kernway-cli 0.3.0
```

## Create a new project

```bash
kernway new my-app
cd my-app
kernway dev
```

Visit [http://localhost:8080/health](http://localhost:8080/health) — if you see `{"status":"UP"}`, you're done.

## Next steps

→ [Your First App](first-app.md)
