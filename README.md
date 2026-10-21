# Obelisk

A Rust document database foundation with folder and S3 storage, designed for
independent concurrent writers. Implementation is in progress.

- [Architecture](docs/PLAN.md)
- [Baseline decisions](docs/DECISIONS.md)
- [Implementation checklist](docs/IMPLEMENTATION.md)

## Development

Rust 1.93 or newer. From the workspace root:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --workspace --no-default-features --locked
```

Commit Cargo.lock for repeatable development. Never put cloud credentials in source.
