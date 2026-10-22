# Obelisk

A Rust document database prototype with local-folder and S3 storage. Independent
writers publish immutable snapshots through conditional updates to one database
head. One engine serves embedded, server, and serverless hosts.

```rust
use obelisk::{Condition, Database, Document, FieldPath, Value};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let db = Database::open_folder("obelisk-data").await?;
let doc = Document::new([
    (FieldPath::parse("profile.name")?, Value::String("Ada".into())),
])?;
let receipt = db.put("request-123", "users/ada", doc, Condition::Any).await?;
let user = db.get("users/ada").await?;
# Ok(())
# }
```

Every successful request ID is bound to its exact canonical operation and original
receipt. Repeating it returns that receipt, even after later edits or deletion.
Reusing it for a different operation fails. After an uncertain outcome, resolve the
ID or retry the same operation; a missing outcome may still be in flight.

`create` requires absence; `put`, `patch` and `delete` accept `Condition::Any`,
`Condition::Absent`, or `Condition::Revision`. Patching an absent document fails.
Deleting a document preserves descendants. For atomic batches and conditions on
other documents, construct a `Transaction` and call `commit`.

Latest reads fetch the authoritative head. Pin `db.snapshot()` for consistent
multiple reads and use its immutable `reference()` to reopen the same snapshot.
All snapshots and request outcomes are retained in this prototype.

## Storage and configuration

The default `folder` feature supports local POSIX filesystems. Windows folder
storage returns an explicit unsupported error; network filesystems are unverified.
OS locks coordinate processes, and durable publication uses file and directory sync.
Readers complete any interrupted HEAD directory sync before returning its snapshot.
Process-kill tests do not prove hardware power-loss behavior.

Enable `s3` and use `Database::open_s3(bucket, prefix)` with external credentials.
For custom providers/builders or retry options, construct a storage adapter and
use `Database::open_with_storage(adapter, DatabaseOptions)`. Each database needs
its own folder or S3 prefix. See [S3 configuration and validation](docs/S3.md).

```sh
cargo run -p obelisk --example local -- /tmp/obelisk-example
cargo run -p obelisk --features s3 --example s3
```

The S3 example requires `OBELISK_S3_BUCKET` and `OBELISK_S3_PREFIX`.

## Current limits

There are no field-filtered queries, compaction, garbage collection, backup/restore, or
network protocol yet. Nothing automatically reclaims committed history, orphaned
uploads, or abandoned temporary files. Immutable segments grow with writes; metadata uses immutable copy-on-write B+
pages with separate document, request,
and parent-membership roots. Opening reads only HEAD and a small root manifest;
point operations load relevant pages. Total metadata is no longer limited by one
64 MiB manifest. Historical pages, receipts, and payloads still accumulate without
compaction or reclamation. One head serializes commits per database and can
cause retries even for disjoint writes.

Values and field paths are validated, with fixed initial resource limits described
in [baseline decisions](docs/DECISIONS.md). Formats are versioned but prototype
versions have no automatic migration. Acknowledgement follows durable publication;
caches are not required for correctness. Real S3 and platform-specific durability
claims require the validation evidence recorded in the checklist.

## Development

Rust 1.93 or newer. Commit Cargo.lock for repeatable development.
See [developer instructions](AGENTS.md) for crate boundaries and commit invariants.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --workspace --no-default-features --locked
```

- [Architecture](docs/PLAN.md)
- [Baseline decisions](docs/DECISIONS.md)
- [Persistent format](docs/FORMAT.md)
- [Validation and measured baseline](docs/BASELINE.md)
- [Implementation checklist and verification gaps](docs/IMPLEMENTATION.md)

## Paged-format compatibility and limits

Fresh databases use HEAD/root format v2. Existing v1 databases fail explicitly
before writes; opening never migrates or resets them. Preserve old data and use a
fresh folder/prefix for v2. Old binaries reject v2 HEAD before transactions.
Document record and canonical transaction fingerprint encodings remain unchanged.

`Snapshot::revision(&path).await?` now returns an owned `Option<Revision>` because
metadata loads lazily. `DatabaseOptions.resources` accepts validated limits for
metadata page reads, metadata transfer, decoded allocation, and prepared metadata.
Defaults are 4096 page reads and 64 MiB for each byte budget; lower nonzero limits
are allowed. Hard page/root/receipt bounds remain independent of these options.
Parent membership is maintained atomically with document mutations. Scalar field indexes remain separate subsequent work.

## Bulk reads and hierarchy discovery

`db.get_many(&["a", "missing", "a"]).await?` preserves input order, duplicates,
and missing entries. Use `get_many_paths` for structured paths. Both accept at
most 256 inputs and fail rather than return a partial vector on resource errors.
All inputs are validated before storage reads, and each bulk read uses one snapshot.

```rust
# async fn example(db: &obelisk::Database) -> obelisk::Result<()> {
use obelisk::{PageOptions, QueryScope};
let scope = QueryScope::path("projects")?;
let options = PageOptions::new(50)?;
let mut cursor = None;
loop {
    let page = db.children(&scope, options, cursor.as_ref()).await?;
    for entry in page.entries {
        println!("{} @ {}", entry.path, entry.revision);
    }
    cursor = page.next;
    if cursor.is_none() { break; }
}
# Ok(())
# }
```

Use `descendants` for all greater depths and `QueryScope::Root` for the virtual
root. Missing ancestors are not synthesized. Deleting a parent preserves its
descendants. Results follow structured path order with escaped boundaries intact.
The same methods exist on `Snapshot` for explicitly pinned reads.

Pages default to 100 entries, accept at most 256, and bound encoded document-entry
bytes to 16 MiB; `with_byte_limit` can lower that budget. Aggregate decoded and
metadata-work limits also apply. Reads currently execute sequentially, within the
maximum allowed concurrency of eight. A page may be shorter due to its byte limit;
a document too large for an empty page returns a resource error.

`QueryPage` contains its snapshot reference, entries, and optional opaque Rust
`QueryCursor`. Continuations retain their original snapshot even when resumed
through a reopened database after writes. Page size/byte budgets may change;
query scope and kind may not. Cross-database or mismatched-snapshot tokens fail
explicitly. Cursors have no public serialization/decoder and do not expire while
history is retained. They need no server-side session. Filtering and field ordering
require the subsequent scalar-index block.

Run the complete facade example against a v2 POSIX folder:

```sh
cargo run -p obelisk --example queries -- /tmp/obelisk-queries
```

The release scaling workload measures live documents and retained request history:

```sh
OBELISK_BASELINE_BACKEND=folder cargo run --release -p obelisk --example scaling --locked
```

See [qualification measurements](docs/BASELINE.md) for the 100,000-document run,
capacity beyond the former manifest bound, and current small-operation costs.
Optional `diagnostics::InstrumentedMetadata` counters distinguish metadata families
from payload reads; they measure logical bytes, not HTTP requests or allocator RSS.
Actual AWS S3 qualification still requires external bucket/prefix configuration.
