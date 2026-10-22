# Obelisk development

Read `docs/PLAN.md`, `docs/DECISIONS.md`, and `docs/IMPLEMENTATION.md` before
changing behavior. Confirmed plan requirements are authoritative. Record decisions,
completed cycles, actual validation, limitations, and remaining work in the docs.

## Architecture and invariants

- `obelisk-types` owns validated public data and stays independent of persistence.
- `obelisk-storage` owns bytes, immutable objects, opaque storage versions, and
  conditional head publication. It must not depend on `obelisk-types`.
- `obelisk-engine` owns versioned encodings, snapshots, transactions, retries, and
  persisted request outcomes. It depends on types and storage.
- `obelisk` is the thin application facade. Examples should depend on it.
- Independent processes must remain safe without a permanent writer or cache.
  In-process locks alone do not establish storage atomicity.
- Prepare durable objects before publishing HEAD. A returned head must itself be
  durable, including recovery from a publisher killed before directory sync.
- Recompute transactions against the latest snapshot after a head conflict.
  Deduplicate requests before checking revision conditions; bind IDs to canonical
  operation fingerprints and retain the original receipts.
- Never use logical paths as filesystem paths or reclaim historical/orphaned data
  without a retention protocol. Indexes, compaction, and reclamation are deferred.

## Delivery and validation

Work in bounded cycles. Preserve unrelated changes, update the implementation
checklist, and commit each verified cycle separately. Default to local commits;
push or publish only when requested. Keep backend features independently buildable.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy -p obelisk --all-targets --locked -- -D warnings
cargo clippy -p obelisk --no-default-features --features s3 --all-targets --locked -- -D warnings
cargo check --workspace --no-default-features --locked
```

See `docs/BASELINE.md` for measurements and subprocess recovery scenarios. Run the
opt-in actual-S3 tests in `docs/S3.md` only with external credentials and a configured
dedicated bucket/prefix. Never embed credentials. Distinguish memory-adapter and
emulator tests from actual AWS S3, and process kills from hardware power loss.
