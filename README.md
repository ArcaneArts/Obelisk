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

Scalar indexed queries are available. Compaction, garbage collection, backup/restore
and a network protocol remain unimplemented. Nothing automatically reclaims committed history, orphaned
uploads, or abandoned temporary files. Immutable segments grow with writes; metadata uses immutable copy-on-write B+
pages with document, request, parent-membership and index-catalog roots. Opening reads only HEAD and a small root manifest;
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

Fresh databases use HEAD/root format v3. Existing v1/v2 databases fail explicitly
before immutable-object preparation or publication; opening never migrates or resets
them. Preserve old data and matching binaries, and use a fresh folder/prefix for v3.
Old binaries reject v3 HEAD before transactions. A binary downgrade does not roll
a v3 database back to its older format.
Document record and canonical transaction fingerprint encodings remain unchanged.

`Snapshot::revision(&path).await?` now returns an owned `Option<Revision>` because
metadata loads lazily. `DatabaseOptions.resources` accepts validated limits for
metadata page reads, metadata transfer, decoded allocation, and prepared metadata.
Defaults are 4096 page reads and 64 MiB for each byte budget; lower nonzero limits
are allowed. Hard page/root/receipt bounds remain independent of these options.
Parent membership and every maintained scalar generation change atomically with
matching document mutations. Opening still reads only HEAD and the small root;
catalogs and scalar-entry trees load lazily.

## Scalar index lifecycle

`create_index(request_id, definition)` registers a new building generation.
`get_index(&id)` and `list_indexes()` return committed definitions, active handles,
candidate progress and failures; the same inspection methods exist on `Snapshot`.
`list_indexes` returns at most 16 entries from one snapshot.

`advance_index_build(request_id, &handle, options)` performs one bounded step.
The pinned source snapshot enumerates paths; each step indexes their current
values. Writers maintain both active and building generations, including new
paths absent from that source. Source exhaustion atomically activates the candidate.
Stop a worker between calls and reopen the database to resume; no background
process or worker lease is required.

Use a new request ID for each successful next step. Repeating an ID with the same
operation returns its original receipt, including the old state/progress after
later edits, activation or drop. Reusing it with other arguments fails. After an
uncertain response, call `resolve_index_operation(request_id)` or repeat exactly
the same operation. A missing result does not exclude an in-flight operation.
Document and management operations share the request namespace; the wrong resolve
method returns `RequestKindMismatch`.

`IndexBuildOptions` defaults to 100 examined source documents and 16 MiB source
bytes, caps documents at 256, and accepts lower limits. Progress counts examined
source paths, even if their current document is missing or not eligible. A byte
boundary stops before the next input; aggregate-resource failures publish no
progress. Inspect a Failed candidate's path/reason, repair the document and call
`rebuild_index` with its handle. Oversized scalar tuples are never truncated or
hashed. Writes producing an unindexable eligible key fail atomically while its
index is building/active. Repairing or deleting an oversized pre-build value is
allowed; failed candidates impose no ongoing write maintenance.

`rebuild_index(request_id, &handle)` creates a replacement generation of the same
definition while the active one continues receiving writes. `drop_index` removes
both current generations without reclaiming historical objects. Recreation uses
a fresh generation; stale handles fail. Definitions are immutable within a
generation. There is one building candidate per database and at most 16 live
index definitions. `DatabaseOptions.indexes = IndexLimits::new(n)?` may lower the
live limit. Index operations and writes fail explicitly when the configured limit
cannot cover committed indexes; they never skip maintenance.

Scalar definitions support exact children/descendants scopes, one to eight exact
string/integer/double/boolean components, directions and optional covered fields.
Missing/type-mismatched components omit an entry. Covered fields retain binary and
typed-list values in separate bounded immutable objects. Filtered/ordered queries
use active generations as described below; expanded independent-process indexed
interruption and scaling qualification follows in cycle 18. The v3 lifecycle does not enable reclamation,
compaction, portable cursors or a wire protocol.

## Indexed queries

`IndexedQuery::new(scope, equality, range, order, projection)` validates one exact
query shape. Equality predicates name a leading composite prefix in index-field
order. Their `Value` determines the exact scalar type; integer and double stay
distinct. `order` lists every remaining component with its type and direction.
An optional `ScalarRange` applies to its first component, with included/excluded
or unbounded endpoints. Lower/upper describe value order even for descending
indexes. Inverted or exclusively empty ranges return empty results.

Use `db.query(&query, options, cursor)` or `snapshot.query(...)`. The planner
requires an active index with exactly the same hierarchy scope and key shape;
extra unused key fields are rejected because they could omit missing-field
records or change tie order. Equal scalar tuples use ascending structured document
paths. Equality-prefix directions do not affect eligibility; remaining directions
must match the index. There is no implicit scan, sort or incomplete-index fallback.
`IndexUnavailable` returns the required signature, requested ID and reason.

Selection uses the smallest compatible active index ID. `query.with_index(id)`
selects a particular compatible index, for example one with needed covered fields.
`Projection::Full` returns documents. `Projection::fields(paths)` canonicalizes
selected fields, preserves types and omits absent fields; an empty selection returns
empty documents with paths/revisions. Key and covered values satisfy projections
without full payload reads. Uncovered fields fetch from the same pinned snapshot.
Query specifications are bounded to 64 KiB and projections to 1024 field paths;
existing index-shape, document, key and aggregate resource bounds also apply.

`PageOptions` defaults to 100 results, caps at 256 and 16 MiB response bytes, and
accepts lower limits. A result that cannot fit an empty page fails explicitly.
Metadata lookahead determines continuation without another full payload read.
For uncovered projections, a full-record size bound can end a page conservatively;
the next page still permits a small projection of a large document. Pages need
not fill their item or byte limit.

`IndexedQueryPage` reports the snapshot, selected generation, results and an opaque
native `IndexedQueryCursor`. A continuation binds canonical query/projection,
explicit index selection, database, snapshot, generation, ordering version and
last emitted key. Page limits may change; mismatched queries, snapshots or positions
fail explicitly. Continuations through a reopened engine retain their original
snapshot after writes, rebuild or drop. Cursors have no portable serialization,
public decoder, expiration or server-side session.

The facade example creates an index, advances one bounded step, closes/reopens,
resumes, resolves/replays the original management outcome, queries, rebuilds and
drops while continuing a pinned query:

```sh
cargo run -p obelisk --example indexed --locked -- /tmp/obelisk-indexed-v3
```

Use a fresh folder. The example can run twice there; each run has fresh management
request IDs and replays the original document seed receipt. It builds with either
backend alone. For an externally configured dedicated fresh S3 prefix, select
`OBELISK_EXAMPLE_BACKEND=s3` and supply `OBELISK_S3_BUCKET` and
`OBELISK_S3_TEST_PREFIX`, then run with `--no-default-features --features s3`.
See S3.md for qualification requirements; compiling this example is not AWS evidence.

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
history is retained. They need no server-side session. Use the scalar-query API
below for filtering and field ordering.

Run the hierarchy facade example against a fresh v3 POSIX folder:

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
