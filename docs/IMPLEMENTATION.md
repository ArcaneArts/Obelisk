# Implementation checklist

Each completed cycle is committed separately. See git history for exact commits.

- [x] 1. Workspace, crate boundaries, CI, and baseline decisions.
- [x] 2. Validated public data model.
- [x] 3. Storage contract and folder adapter.
- [x] 4. S3 adapter and opt-in real-service contract tests (execution pending).
- [x] 5. Persistent formats and metadata-only snapshot opening.
- [x] 6. Atomic transactions and conditional publication.
- [x] 7. Persisted idempotency and uncertain-outcome resolution.
- [x] 8. Public facade, examples, and usage documentation.
- [x] 9. Multiprocess/crash harness, local validation and measurements.
- [x] 10. Recover interrupted folder publication before acknowledging observed heads.
- [x] 11. Immutable ordered metadata pages, root and receipt primitives.
- [x] 12. Paged snapshots, transactions and atomic hierarchy membership.
- [ ] 13. Bulk reads and bounded hierarchy APIs.
- [ ] 14. Pagination, multiprocess/crash and scaling qualification.

## Evidence

Cycle 1: workspace formatting, tests, lint, and no-default-feature build checked
before commit. The initial libraries intentionally only establish crate boundaries.

Cycle 2: six model tests pass, including generated structured-path round trips,
field conflicts, typed empty/nested lists, invalid doubles, and duplicate batches.
Workspace tests, formatting, strict Clippy, and no-default-feature check pass.

Cycle 3: shared storage contract and 16-way folder initialization race pass on
macOS. Immutable overwrite protection, range bounds, one-winner head CAS, listing,
and idempotent deletion are exercised. Workspace tests and strict Clippy pass;
no-default-feature build passes. Folder adapter explicitly rejects non-POSIX hosts.

Cycle 4: object_store 0.12.5 conditional-write source inspected; S3 adapter forces
ETag-match publication. Two local adapter tests, all workspace tests, formatting,
strict Clippy and no-default-feature build pass. Real S3 contract is explicitly
ignored by default and was not run: no OBELISK_S3_BUCKET/test-prefix configuration
or AWS environment configuration was supplied. Actual S3 verification remains open.

Cycle 5: explicit checksummed v1 encodings and metadata-only snapshot reads pass
round-trip, corruption/version, hostile-length, generated scalar, and arbitrary-byte
decoder tests. A database with a corrupt payload opens successfully from metadata
and reports corruption on the targeted read. Workspace tests, strict Clippy, and
no-default-feature build pass. Historical snapshots are retained without reclamation.

Cycle 6: atomic CRUD/patch batches, missing-document patches, create-if-absent,
revision and explicit read conditions, pinned snapshots and reopen checks pass.
Twelve concurrent disjoint writers preserve all commits. Full tests, formatting,
strict Clippy and no-default-feature check pass. Request deduplication is cycle 7.

Cycle 7: canonical operation fingerprints and original receipts persist in the
committed manifest. Twelve engine tests pass, including concurrent duplicate
requests, reordered equivalent batches, payload reuse rejection, reopen recovery,
lost success responses, lost resolution reads, and hidden-success conflicts with
a one-attempt budget. Full tests, strict Clippy and feature checks pass.

Cycle 8: public facade lifecycle integration test passes. Local example runs
against a real folder; S3 example compiles but is not executed without external
configuration. Full workspace tests, formatting, strict Clippy, and feature checks
pass. README describes operation semantics, limits, retained history and usage.

Cycle 9: local multiprocess harness verifies simultaneous initialization, six
independent writers publishing 24 atomic pairs, six identical requests committing
once, and four contending writers preserving 16 conditional increments. Nine
commit-boundary process kills preserve acknowledged seed data, atomic visibility,
and persisted retry receipts. A separate kill verifies OS-lock release on death.
Deterministic same-document head conflicts recompute patches without losing fields.
Additional tests cover delete/recreate revision identity, list-depth and document
resource bounds, and malformed checksummed payloads. Limits are checked while
preparing batches, metadata is validated against reader limits before publication,
and public snapshot references are opaque.

Cycle 9 checks rerun on 2026-10-22 in an isolated worktree: 29 passed; two actual-S3
tests explicitly ignored. Formatting, strict Clippy with all features, facade
folder-only and S3-only Clippy, and the
no-default-feature workspace build pass. The release-mode baseline and its limits
are recorded in [BASELINE.md](BASELINE.md). Logical adapter counters are available
through `obelisk::storage::metrics::InstrumentedStorage`.

Cycle 10: folder head reads now finish directory sync under a shared publication
lock before returning a visible head, including a publisher killed between rename
and sync. A real subprocess is killed at that exact adapter-internal boundary;
a reader opened before the crash recovers the head and permits the next CAS.
Injected sync failures prove that visible bytes alone cannot yield a successful
head read. Relative roots are pinned at open. Developer instructions document the
crate boundaries and commit, durability, and qualification rules.

Cycle 10 checks on 2026-10-22: 33 workspace tests passed (including two subprocess
worker entry points), with two actual-S3 tests ignored. Formatting, strict workspace
and facade feature Clippy, and no-default-feature build all passed. The local example
ran twice against one folder and returned the same persisted commit receipt; the
S3-only example compiled. Baseline results are recorded separately; no hardware
power-loss or actual-S3 claim is made.

## Cycle commits

| Cycle | Commit | Capability |
| --- | --- | --- |
| 1 | `6b11aaf` | Workspace and decisions |
| 2 | `628ef80` | Validated public model |
| 3 | `f80e6e2` | Folder adapter and shared contract |
| 4 | `c5bd30c` | S3 adapter and opt-in contract |
| 5 | `2d1d35d` | Formats and snapshots |
| 6 | `6e194cf` | Atomic transactions |
| 7 | `ffbbd97` | Idempotency and uncertain outcomes |
| 8 | `afba0e6` | Public API and examples |
| 9 | `250ba09` | Multiprocess/crash harness, counters and measurements |
| 10 | This document's commit | Durable recovery of observed folder heads |

## Acceptance still open

The local baseline is implemented and verified on macOS. Overall acceptance is
**incomplete** until the actual-S3 storage contract and multiprocess recovery suite
run successfully with a configured isolated bucket prefix and external credentials.
No test bucket/prefix was supplied in this session; neither real S3 nor an emulator
was exercised. The S3 source and adapter logic were inspected/tested locally.

CI definitions cover Linux, macOS and Windows, but remote CI has not run in this
session. Folder durability is limited to local POSIX filesystems; Windows folder
opening explicitly returns unsupported. Network filesystems and hardware power-loss
durability remain unverified. Process termination tests cover engine operation
boundaries and the folder rename-before-sync window, not every instruction within
filesystem or transport implementations.

History, orphaned uploads, request outcomes, and abandoned temporary files are not
automatically reclaimed. Paged metadata now removes whole-manifest rewrites; single-head contention and
retained-history growth still limit the prototype. Public discovery queries, field indexes,
compaction, reclamation, backup/restore and transport remain deferred as scoped.

Next milestones: expose bulk/hierarchy discovery and complete pagination/crash/scale
qualification. Actual-S3 qualification remains parallel and externally configured.
Scalar field indexes follow query foundations; compaction/reclamation remain deferred.

## Query foundations cycle 11 evidence

The additive metadata primitives provide ordered document/membership codecs,
checksummed family-specific B+ pages, fixed-size root references, separate large
receipt objects, bounded point/range reads, and coalesced copy-on-write batch
updates with sibling rebalancing and root collapse. Only reachable prepared pages
are uploaded. Existing pages and historical roots remain immutable and unreclaimed.
At cycle 11 the production writer remained v1; paged integration and public
query APIs belonged to subsequent cycles 12–14.

Local validation on 2026-10-22: 42 workspace tests passed and two actual-AWS-S3
tests were explicitly ignored. Formatting, strict workspace all-feature Clippy,
facade folder-only and S3-only Clippy, and the no-default-feature workspace check
passed. Nine added tests include generated reference-map operations, immutable
old roots, ordered-key properties, arbitrary/checksummed malformed inputs, resource
failures before reads, large receipts, maximum paths across multiple tree levels,
shuffled deletion/fanout checks, bounded range continuation, and subtree reuse.
These tree tests use an in-memory byte adapter and do not qualify actual S3.
No performance or hardware-power-loss claim is made. Actual S3 remains pending.

Cycle 11 remote delivery: PR #2 merged, with Linux, macOS, and Windows CI checks
observed passing. Its clean worktree remains temporarily registered because
Cantrip refused removal due to a reported binding; no forced cleanup was used.

## Query foundations cycle 12 evidence

Fresh databases use HEAD/root v2 with independently addressable location, outcome,
and parent-membership trees. Opening reads only HEAD and the bounded root. Existing
v1 databases fail explicitly before writes. Document records and canonical
fingerprints retain v1 encodings. Revision lookup is async and owned; resource
limits are configured through `DatabaseOptions.resources`.

Transactions prepare immutable payloads, receipt objects, changed tree pages and
root before conditional publication. Membership changes atomically with document
existence; patches preserve unchanged membership pages. The internal publisher
also supports metadata-only roots while preserving document revisions and original
request receipts. Historical references remain opaque and retained. Conflict
retries recompute from fresh roots, with deduplication before conditions.

Local validation on 2026-10-22: 46 workspace tests passed, with two actual-S3 tests
explicitly ignored. Formatting, strict workspace all-feature Clippy, facade
folder-only and S3-only Clippy, and no-default-feature workspace checking passed.
New integration evidence includes root-only startup despite corrupt document
reads, full 256-mutation batches with 4004-byte paths and receipts larger than a
page, split/collapse/reopen history, metadata-only publication and stale-root CAS
rejection, atomic membership transitions, explicit v1 rejection, and resource
failure without publication. The folder subprocess suite now reaches 13 commit
boundaries using object-kind classification, including page and receipt uploads.
The adapter's rename-before-directory-sync recovery tests remain passing.

No actual AWS S3 or hardware power-loss validation ran. Public bulk/hierarchy
APIs and scaling qualification remain cycles 13–14. Total retained data and
single-HEAD contention remain unbounded operational concerns.
