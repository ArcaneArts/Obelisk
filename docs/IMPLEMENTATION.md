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
- [x] 13. Bulk reads and bounded hierarchy APIs.
- [x] 14. Pagination, multiprocess/crash and scaling qualification.
- [x] 15. Validated scalar-index definitions and ordered metadata primitives.
- [x] 16. Root-integrated resumable index build, reconciliation and activation.
  - [x] 16A. Additive lifecycle types and bounded v3/entry/projection/outcome codecs.
  - [x] 16B. Authoritative v3 integration, lifecycle APIs and mandatory writer maintenance.
- [ ] 17. Bounded indexed query APIs over atomically maintained generations.
- [ ] 18. Index concurrency, interruption and scaling qualification.

The scalar-index Task spans 16A, 16B, 17 and 18 as independently merged milestones.
Cycle 16 is delivered by the 16B merge, including mandatory writer maintenance.
Cycle 17 exposes queries over that maintained state. Later roadmap
features (compaction, reclamation, backup/restore and transport) are separate Tasks.

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
No external AWS test bucket/prefix was supplied. MinIO HTTP emulator evidence is
recorded below; it does not close the real-service gate.

Linux, macOS and Windows CI passed on query-foundation PRs #2, #3 and #4. The
cycle-14 PR carries its own CI results. Folder durability is limited to local POSIX filesystems; Windows folder
opening explicitly returns unsupported. Network filesystems and hardware power-loss
durability remain unverified. Process termination tests cover engine operation
boundaries and the folder rename-before-sync window, not every instruction within
filesystem or transport implementations.

History, orphaned uploads, request outcomes, and abandoned temporary files are not
automatically reclaimed. Paged metadata now removes whole-manifest rewrites; single-head contention and
retained-history growth still limit the prototype. Scalar lifecycle and maintenance
are delivered in cycle 16; indexed queries and qualification remain cycles 17–18.
Compaction, reclamation, backup/restore and transport remain deferred as scoped.

The four query-foundation implementation milestones are complete locally.
Actual-S3 qualification remains parallel and externally configured. Scalar-index
lifecycle and atomic maintenance are complete in cycle 16; indexed queries and
qualification remain cycles 17–18. Compaction/reclamation remain deferred.

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

Cycle 12 remote delivery: PR #3 merged, with Linux, macOS, and Windows CI checks
observed passing. Primary was synchronized and the cycle's worktree/branch removed.

## Query foundations cycle 13 evidence

The facade and snapshots expose ordered bulk reads and bounded children/descendant
pages, including virtual root. Bulk reads preserve duplicates and missing entries;
string inputs and count bounds are checked before storage reads. Children seek
exact membership scopes, descendants seek structured location-key prefixes, and
neither synthesizes missing ancestors or includes the scope document.

Opaque typed cursors bind committed snapshot, query kind/scope, ordering version,
and last emitted key. They can resume through a reopened engine after writes;
page budgets may change without changing query identity. Mismatched database,
snapshot, query, version or position yields an explicit cursor error. Returned
values, parsed paths, bulk clones, and cursor identities receive memory charges.
A metadata-only lookahead avoids fetching an extra document payload. No portable
cursor decoder, transport, field filtering, or reclamation was added.

Local validation on 2026-10-22: 54 workspace tests passed and two actual-S3 tests
were explicitly ignored. Formatting, strict workspace all-feature Clippy, facade
folder-only and S3-only Clippy, and the no-default-feature workspace check passed.
Six facade query tests and native/type tests cover structured reference results,
missing parents, escaped boundaries, parent deletion, bulk duplication/missing
entries, invalid input before I/O, snapshot-pinned resumption after writes and
reopen, cursor mismatches, and byte-limited pages without skipped prefetches.
The facade query example ran twice against one temporary POSIX folder and returned
the same persisted revisions, demonstrating request deduplication on reopen.

Multiprocess query visibility, larger scaling/capacity measurements, and the final
acceptance audit remain cycle 14. Actual AWS S3 qualification remains pending.

Cycle 13 remote delivery: PR #4 merged, with Linux, macOS and Windows CI checks
observed passing. Primary was synchronized and the cycle's worktree/branch removed.

## Query foundations cycle 14 evidence

Optional engine diagnostics distinguish metadata families, receipt objects, and
payload ranges without adding query semantics to storage. The new release scaling
example and the existing baseline build independently for folder and S3. Both
report logical adapter operations rather than physical HTTP requests. No cache is
required or enabled for structural qualification.

Tree updates now leave unrelated sparse fringe pages untouched; a reproducer
failed with four uploads before the fix and passes with exactly the changed leaf
and root afterward. A descriptor/header count check precedes page entry allocations.
Additional checksummed malformed-page tests cover semantic corruption and missing
children. Lost request-page reads after a concealed successful publication retain
an uncertain outcome until later resolution returns the original receipt.

Final local checks on 2026-10-22: **59 workspace tests passed, three ignored**.
The ignores are two actual-AWS tests and the explicit large capacity workload.
Formatting, strict all-feature workspace Clippy, folder-only and S3-only facade
Clippy, and no-default-feature workspace checking passed. An explicit S3-only
check of both measurement examples also passed. The release capacity test ran
separately and passed in 309.80 seconds: 100,000 documents, 91,200,000 actual live
location key/value bytes, a 381-byte root, and five location pages for one update.
It also verified reopening, original receipts, and historical reads after edits.

The recorded POSIX multiprocess run completed six disjoint writers/24 atomic pairs,
six identical requests committing once, and four contending writers/16 conditional
increments. Four hierarchy writers completed 36 outcomes while readers performed
343 pinned scans; document values/revisions matched immediate membership, parent
deletion preserved leaves, and pinned page resumption survived concurrent writes.
All 24 intended commit-boundary kill markers were reached, including five
256-document split-producing batches. Documents, hierarchy entries, and request
outcomes appeared together or remained absent together. OS-lock death and the
adapter's rename-before-directory-sync recovery tests also passed.

The full release workload reached 100, 1,000, 10,000 and 100,000 live documents
and 100, 1,000 and 10,000 distinct requests repeatedly updating one document.
Every open transferred 795 logical bytes and read no tree/payload/receipt objects.
Point/range work followed tree height, and replacing documents created no membership
pages. Small-dataset writes were slower than v1 because of additional object
operations; repeated point traversals in bulk/children remain measurable overhead.
Retained storage still grows, and contention still serializes through HEAD.
Commands, environment, full counters, distributions, and before/after comparisons
are in [BASELINE.md](BASELINE.md) and its linked measurement output.
An additional contended baseline reconciles per-writer immutable uploads against
global counters: 802,620 bytes for 40 successful attempts versus 3,541,629 bytes
abandoned after 187 conflicts, with zero pending/unclassified uploads. This
benchmark accounting changes no engine/storage publication behavior.

### Completed-plan acceptance audit

| Criterion | Evidence |
| --- | --- |
| Lazy startup, relevant-page work, local updates | Root-only counters; selective one-child query with 3,072 deeper documents; exact unchanged child references; 100,000-document scaling and capacity runs |
| Ordered, bounded, immutable metadata | Reference-map randomized edits/scans; split/merge/collapse, maximum paths, large receipts, hostile bytes/checksummed semantics, family/version/level/separator validation, aggregate budgets |
| Bulk/root/hierarchy semantics | Type and facade reference tests for ordered duplicate/missing inputs, virtual root, missing ancestors, escaped boundaries, parent deletion, persisted membership |
| Snapshot pagination and mismatch errors | Query tests across insert/update/delete and reopen; typed query/database/snapshot/version/position mismatch checks; 16 MiB pages resume without skipping; independent-process pinned continuation |
| Atomic publication and preserved write guarantees | Expanded process-kill and hierarchy suites; original receipt/fingerprint/revision tests; lazy resolution-read faults; final lookup after conflicts; durable observed-HEAD tests |
| Metadata-only publication and compatibility | Replacement pages in all three trees preserve revisions/receipts; stale roots fail CAS and retries rebuild from fresh state; lost publication responses remain explicit; v1 opening rejects before writes; opaque references and retained history |
| Resource and former-capacity bounds | Low decoded/page/response budgets fail explicitly; checksums/counts fail before entry allocation; live location metadata exceeds 64 MiB; all required local checks pass |
| Developer experience and rollout | Thin facade and runnable query example; async revision and v2 fresh-folder/prefix break documented; no migration, transport, reclamation, or scalar indexes introduced |
| AWS qualification status | Pending dedicated external bucket/prefix; no actual AWS or emulator run, no remote cleanup or embedded credentials |

No hardware power-loss, network-filesystem, Windows-folder durability, bounded
history retention, or universal latency claim follows from these results.

### Query-foundations metadata rewrite qualification

The metadata-only acceptance test replaces document-location, parent-membership,
and request-outcome pages through the common internal publisher. Its 256-document
fixture spans multiple pages. Complete paginated children and descendant results,
document revisions, original application receipts, and retained snapshots survive
the rewrite and reopening. A concurrent document commit rejects the stale rewrite
by CAS; rebuilding from authoritative state preserves that commit's document,
membership and outcome on retry. The whole-fixture rebuild is test-only; ordinary
transactions still edit relevant tree paths.

A separate storage fault loses the response after successful metadata-only HEAD
publication. The test requires an explicit uncertain storage error and confirms
that the intended CAS was reached. Reopening observes the new commit identity,
unchanged query results and original receipts. Instrumentation proves this rewrite
creates all three metadata families and a root, without creating document segments,
document records or receipt objects. These are local POSIX tests, not actual AWS
or hardware power-loss qualification.

Validation on 2026-10-22: both focused metadata-only tests passed; the full workspace
run passed **70 tests with three ignored**. Formatting, strict all-feature workspace
Clippy, folder-only and S3-only facade Clippy, and no-default-feature workspace
checking passed. The ignored tests are the two external AWS gates and the large
release-only capacity workload; prior capacity/scaling evidence above is unchanged.
PR #5's Linux, macOS and Windows CI checks were also observed passing during this
audit. Actual AWS qualification remains pending a dedicated external bucket/prefix
and credential provider.

## Scalar indexes cycle 15 evidence

Public types now validate stable index identities, exact children/descendants
scopes, one to eight ordered string/integer/double/boolean components, and up to
32 canonical covered fields. Definitions reject duplicate or impossible component
paths and enforce an aggregate byte bound. Engine primitives encode checksummed
definitions, distinct definition/entry page families, bounded composite prefixes
and complete keys, and document-path tie-breakers. Missing fields, type mismatches
and out-of-scope documents produce no entry. Oversized scalar tuples fail without
allocating beyond the key bound or emitting a truncated key.

Ordering tests cover both directions, embedded NUL strings, integer extremes,
finite-double extremes and generated integer/double pairs. Composite round trips,
structured path ordering, scope/type omission, family mismatches, future versions,
noncanonical negative zero, hostile keys, arbitrary bytes and definition limits
also pass. Existing v2 HEAD/root bytes and production reads/writes are unchanged;
the new page families have no authoritative root reference and cannot yet become
query-visible. Covered projection values are not encoded yet.

Local validation on 2026-10-22: **69 workspace tests passed, three ignored**. The
ignores are two actual-AWS tests and the explicit large capacity workload. Formatting,
strict all-feature workspace Clippy, folder-only and S3-only facade Clippy, and the
no-default-feature workspace check passed. No actual AWS S3, emulator, performance,
multiprocess-index, process-kill-index or hardware-power-loss qualification ran.
The next cycle must define root-persisted lifecycle state and resumable base-build,
concurrent-change reconciliation and atomic activation before indexes can be used.

## MinIO emulator qualification cycle

Separate explicitly ignored emulator entry points reuse the byte-storage contract
and complete multiprocess/query/crash harness without weakening the actual-AWS
endpoint guards. Production storage and engine behavior are unchanged. A dedicated
local bucket and random externally supplied credentials were used; no credentials
are committed. UUID database prefixes and objects are retained without cleanup.

On 2026-10-23 both S3-only emulator tests passed through HTTP against the local
MinIO development image identified in
[the evidence log](measurements/minio-2026-10-23.txt). Six independent writers
published 24 atomic pairs, six duplicate requests committed once, and four
contending writers preserved 16 increments. Four hierarchy writers published
36 outcomes during 13 pinned scans. All 24 intended crash markers were reached,
including split-producing batches; documents, memberships and original outcomes
remained atomically visible after reopening. This kills client processes, not the
MinIO server, and does not validate hardware power loss.

The S3-only release scaling smoke passed at 100 and 1,000 live documents and
100 retained requests on one document. Opening transferred 795 logical bytes at
every checkpoint, with no tree, payload or receipt reads. At 1,000 documents a
single replacement created six objects and uploaded 20,121 bytes. Full commands,
prefixes, counters and distributions are retained in the evidence log and S3.md.
This is a small emulator workload, not a substitute for the existing large folder
measurements or full AWS scaling qualification. The server reports
DEVELOPMENT.GOGET without an upstream release identity; evidence applies only to
the recorded image ID. Timings overlapped local compilation/tests and are not a
comparable performance baseline. No service-side HTTP metrics were captured.

Local cycle validation: formatting and diff checks passed; workspace all-feature
tests passed **70 tests with five ignored** (two AWS gates, two emulator gates,
and the explicit large capacity workload). The emulator gates were run separately
and passed. Strict workspace all-feature Clippy, facade folder-only and S3-only
Clippy, and no-default-feature workspace checking all passed. The dedicated MinIO
container is stopped with its database objects retained. Actual AWS contract,
expanded multiprocess recovery and scaling remain pending external credentials
and a dedicated bucket/prefix; overall S3 acceptance remains open.

### Final query-foundations corruption audit

The required cycle case is now explicit: a checksummed, validly referenced branch
points back to its own object while claiming a lower child level. Point and range
traversal reject the child integrity hash after exactly two reads; copy-on-write
editing rejects the inconsistent cached descriptor after one read. No preparation
is published or persisted. A separately checksummed same-level child fails in the
reader after one read, and an insufficient metadata-transfer budget fails before
any storage read. These tests exercise production traversal with an in-memory
byte adapter; they add no storage, format or API changes.

Validation on 2026-10-23: both focused tests passed, as did formatting, **72 workspace
tests with five ignored**, strict all-feature workspace Clippy, facade folder-only
and S3-only Clippy, and no-default-feature workspace checking. The ignores remain
the two external AWS gates, two opt-in emulator gates (separately passed in the
preceding cycle), and the explicit large capacity workload (previous release run
recorded above). PR #8's Linux, macOS and Windows CI checks all passed.

The final plan audit maps the delivered code and evidence to all four milestones:

- Metadata primitives: ordered-key properties, randomized BTreeMap operations,
  split/merge/root-collapse checks, unchanged subtree identities, large receipts,
  checksummed semantic corruption, explicit cycles, and bounded traversal.
- Paged publication: root-only startup counters; async owned revision API; original
  fingerprints/receipts; conflict and uncertain-outcome tests; membership existence
  transitions; equivalent metadata-only rewrites; retained history and v1 rejection.
- Discovery APIs: facade reference-model tests for bulk input order, duplication,
  absence, virtual root, missing parents, escaped boundaries and parent deletion;
  bounded native cursors; a runnable facade example and compatibility docs.
- Qualification: pinned pagination under writes/reopen; independent-process
  hierarchy visibility; all 24 kill markers and the separate folder recovery
  barrier; aggregate metadata beyond 64 MiB; complete local validation and backend
  feature builds; recorded 100/1,000/10,000/100,000-document and increasing request
  histories with successful/abandoned upload accounting and latency distributions.

All four implementation milestones have been delivered. As explicitly permitted
by the approved plan, actual AWS qualification is recorded as pending external
credentials and a dedicated bucket/prefix; emulator results do not close overall
S3 acceptance. Scalar index lifecycle, compaction, reclamation and transport remain
outside the query-foundations Task. HEAD contention and retained-storage growth
remain documented limitations.

## Scalar indexes cycle 16A evidence

Public types validate generation handles, coherent examined-count/source-path
progress, live-index limits and build options. Engine-owned additive codecs cover
v3 HEAD/root, catalog descriptors, generation-bound entry values, out-of-line
typed covered projections and tagged outcomes. Management fingerprints bind their
operation domain, canonical definition or handle, generation and work limits.
Original document fingerprints/receipts and their nonempty revision invariant are
unchanged. Production snapshots and transactions still use v2; no lifecycle or
indexed-query execution is exposed yet.

Eight new engine tests and one type test cover v2/v3 version rejection, root
capabilities/counts, build/rebuild/failure descriptor states, cross-object scope
checks, maximum paths/identities, projections beyond leaf size with binary and
typed nested/empty lists, receipts beyond page size, management operation/state
validation, fingerprint binding, aggregate/low allocation budgets, hostile lengths,
checksum corruption and arbitrary validly checksummed payloads. These are pure
codec/type tests, not independent-process or actual-S3 index qualification.

Local validation on 2026-10-31: **81 workspace tests passed, five ignored**.
Formatting, strict all-feature workspace Clippy, folder-only and S3-only facade
Clippy, no-default-feature workspace checking, and diff checks passed. The ignored
tests remain two externally configured actual-AWS gates, two emulator gates and
the explicit large capacity workload; none of those opt-in workloads ran in 16A.
No performance or hardware-power-loss claim is made.

Cycle 16 stays open. Milestone 16B must select v3 only for fresh folders/prefixes,
integrate catalog/status and idempotent lifecycle APIs, resumable reconciliation
and activation, and maintain both building and active generations atomically on
every document write. Cycle 17 supplies indexed queries and examples; cycle 18
qualifies independent processes, interruptions and scale. Actual AWS acceptance
remains open without a dedicated external bucket/prefix and credential provider.


## Scalar indexes cycle 16B evidence

Production HEAD/root now select v3 and persist the bounded index catalog. Fresh
folders/prefixes are required; v1/v2 rejection occurs before any upload or HEAD
attempt. Opening remains limited to HEAD/root even with an active index. Public
engine/facade methods create, inspect/list, advance, rebuild, drop and resolve
management operations. Snapshots inspect their own retained catalog. Generations,
pinned source references, progress, failures and original outcomes survive reopen.

Online steps enumerate bounded pinned source paths but reconcile current documents.
Every foreground batch updates building and active generations through the same
root as document locations, membership and outcomes. Rebuild maintains both until
atomic activation; drop removes current maintenance without deleting history.
Covered binary/list values live in separate bounded projection objects. Eligible
oversized new keys reject the whole write; oversized pre-existing keys persist an
actionable build failure and permit repair/rebuild. Missing/type-mismatched later
components correctly omit a document before key-size validation.

Forced-CAS tests cover registration versus an already-prepared writer, activation
versus a write, competing builders, and drop/recreate versus a stale builder. Tests
also cover byte-limited progress, updates before enumeration, post-base inserts,
covered-only changes, delete/recreate, stale generations, original request replay,
shared-namespace reuse, all four management operations with concealed success,
failed resolution reads, and a one-attempt concealed-conflict outcome. Checksummed
cross-object identity corruption and impossible source positions reject safely.
Metadata-only publications preserve the catalog, revisions, outcomes and retained
snapshots. Low resource limits reject before upload/publication, including a new
regression that first reproduced initialization publishing a root unreadable under
its own configured limits; initialization and shared publication now validate the
prepared authority against a fresh reader budget before uploads.

Milestone 16A was independently merged as PR #10; Linux, macOS and Windows checks
were observed passing. The unrelated primary manifest-whitespace commit remains
preserved; primary cannot be fast-forwarded to upstream without changing that
history. Cycle 16B uses its own isolated branch based on that merge. Its local
validation and PR delivery do not complete the full scalar-index Task: cycles 17
and 18 remain required.

Local validation on 2026-10-31: **98 workspace tests passed, five ignored**.
All six required checks passed: formatting, all-feature workspace tests, strict
all-feature workspace Clippy, folder-only and S3-only facade Clippy, and
no-default-feature workspace checking. Folder-only and S3-only example builds
also passed. The local facade example ran twice against a disposable POSIX folder
and returned identical persisted receipts and documents. Diff checks passed.
The ignored gates remain two actual-AWS tests, two emulator tests and the large
release capacity workload; none ran in this milestone. The existing non-indexed
folder multiprocess/crash suite passed in the workspace run. Expanded indexed
independent-process/crash and release scaling/capacity evidence remains cycle 18;
no new AWS, emulator, performance or hardware-power-loss evidence is claimed.
