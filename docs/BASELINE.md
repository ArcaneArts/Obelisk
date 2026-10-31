# Validation and baseline measurements

Cycle 10 validation and measurements on 2026-10-22 in an isolated worktree.
Environment: macOS 27.0.1 (26A434), aarch64, rustc 1.93.0, Cargo 1.93.0.
Apple M4 Max, 128 GiB RAM. Release build uses the checked-in lockfile.
The run used a fresh temporary local folder, no engine cache, and no filesystem
cache flushing. Other machine activity was not controlled. It is a single-machine
baseline, not a capacity or regression gate.

## Correctness verification

Final automated run: **33 tests passed**, with **two actual-S3 tests ignored**.
Two passing tests are subprocess worker entry points; their parent tests run the
scenario suites below. Generated tests exercise many inputs per test.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy -p obelisk --all-targets --locked -- -D warnings
cargo clippy -p obelisk --no-default-features --features s3 --all-targets --locked -- -D warnings
cargo check --workspace --no-default-features --locked
```

These checks passed locally. CI is configured for Linux/macOS/Windows but has not
run remotely in this session. Windows folder operations explicitly return unsupported.

Repeat the real folder subprocess scenarios:

```sh
cargo test -p obelisk --all-features --test multiprocess \
  folder_multiprocess_and_crash_recovery -- --nocapture
```

The harness uses separate test processes, per-process ready markers, a shared start
gate, and hard process termination without Rust cleanup. It verifies:

- Simultaneous initialization and six independent writers producing 24 atomic
  two-document batches, checked through pinned snapshots and after reopening.
- Six concurrent identical create requests yielding one committed outcome.
- Four contending processes producing 16 revision-conditional counter increments.
- Process kills after the head read, after snapshot metadata read, before/after
  segment upload, before/after manifest upload, before head publication, after
  publication before returning the storage response, and after acknowledgement.
- Every killed writer preserves the prior acknowledged seed, exposes either both
  batch documents or neither, and safely resolves/retries the same request.
- Killing a process while holding the publication lock permits subsequent commits.

Cycle 10 additionally kills a process inside the folder adapter after HEAD rename
and before directory sync. A reader opened before the kill recovers the head and
successfully publishes against its token. Injected directory-sync failures verify
that visible HEAD bytes cannot be returned successfully until the read barrier
succeeds. This exercises the production publication helper without exposing fault
switches in the public API. Folder roots are also tested for canonical pinning.

Engine tests additionally force two writers to start from the same document
snapshot and verify that conflict retries recompute patches without losing fields.
Transport fault tests cover a lost success response, failed resolution reads, a
prepublication error, and an apparent conflict after a concealed successful write.
They include a one-attempt retry budget and persisted receipts after reopen.

Value/model/format tests cover typed empty and nested lists, resource boundaries,
finite doubles, path escaping and generated round trips, field conflicts,
conditional writes, deletion/recreation revisions, checksums, unsupported versions,
malformed checksummed payloads, and arbitrary decoder inputs. A separate corrupt
payload test confirms that opening reads metadata without fetching document bodies.

## Release workload

```sh
CARGO_TARGET_DIR=target cargo run --release -p obelisk --example baseline --locked
```

The workload writes a 1024-byte binary field to 50 documents, performs a first
measured read and 100 repeated latest reads, reopens the engine and obtains a
snapshot 20 times, then runs eight async writers with five writes each. These
concurrent performance writers share one process; independent-process correctness
is verified separately above. The startup counters are captured while opening a
populated engine through an already-created adapter, excluding adapter setup.

Raw final run:

```text
Baseline: 1024-byte binary payload; 50 sequential writes, 100 latest reads, 20 reopen+snapshot operations, 8 writers x 5 writes.
OS: macos / aarch64; first measured read: 6.755 ms. OS filesystem caches are not flushed.
| Workload | Samples | p50 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: |
| Sequential durable write | 50 | 33.851 | 43.032 | 61.065 |
| Repeated latest read | 100 | 7.286 | 10.145 | 13.553 |
| Reopen + snapshot | 20 | 27.794 | 36.044 | 39.399 |
| Concurrent durable write | 40 | 85.773 | 1926.001 | 2109.745 |
Concurrent CAS attempts: 160; conflicts: 120; acknowledged commits: 40.
Opening populated database (logical adapter operations): Stats { head_reads: 2, object_reads: 1, range_reads: 0, bytes_read: 30516, object_creates: 0, bytes_written: 0, head_cas_attempts: 0, head_cas_successes: 0, head_conflicts: 0, list_calls: 0, delete_calls: 0 }
Stored objects including retained/orphaned versions: 421; bytes: 4063067.
Total logical adapter operations (not physical HTTP requests): Stats { head_reads: 373, object_reads: 453, range_reads: 101, bytes_read: 6218262, object_creates: 421, bytes_written: 4081540, head_cas_attempts: 211, head_cas_successes: 91, head_conflicts: 120, list_calls: 0, delete_calls: 0 }

```

The first measured read does not establish a physically cold disk read. Reopening
uses warm OS caches and includes metadata decoding; it does not replay history or
load documents. Startup fetched two heads and one metadata object, transferring
30,516 bytes with zero document-range reads.

Folder latest reads perform a directory sync under the publication lock on every
head read. This conservative recovery barrier contributes to read/reopen latency;
it also prevents request resolution from acknowledging an unsynced rename. Pinned
snapshot document reads do not repeat that barrier. Cycle 9's measurements before
the barrier are preserved in commit `250ba09`; these runs are not a controlled
performance comparison.

Forty contending writes required 160 CAS attempts and encountered 120 conflicts.
The long tail illustrates the single-head publication bottleneck and bounded
backoff. Conflict counts and latencies vary with scheduling. No throughput target
or optimization gain is claimed.

The 421 stored objects include retained historical segments/manifests and orphaned
preparations from conflicts. Ninety live 1024-byte payloads are intentionally a
small dataset; the approximately 3.9 MiB footprint includes full metadata rewrites.
Object growth and retry amplification justify follow-up metadata/maintenance work.

## Remaining qualification

Real S3 contract, independent-process recovery, latency, and request-cost measurements
have not run. No test bucket/prefix was configured. See [S3.md](S3.md) for opt-in
commands, including the same release workload on an isolated S3 prefix. Logical
adapter counters cannot substitute for physical S3 HTTP-request metrics.

Process kills validate restart behavior at engine operation boundaries and the
folder rename-before-sync window. They do not simulate hardware power loss, every
instruction inside an adapter operation, or
filesystem/controller failures. Network filesystems and other OS durability behavior
remain unverified. Overall backend acceptance remains open pending actual S3 results.

## Query foundations qualification (cycle 14)

Measurements on 2026-10-22: macOS 27.0.1 (26A434), Apple M4 Max / aarch64,
128 GiB RAM, rustc 1.93.0 and Cargo 1.93.0, checked-in lockfile, release builds.
Fresh disposable POSIX folders; no engine cache or filesystem-cache flushing.
Other machine activity is not controlled. These are observed runs, not universal
latency thresholds, physical cold-disk measurements, or AWS qualification.

### Comparable small baseline

The same `baseline` example ran on the retained cycle-11 checkout (production v1
writer) and cycle-14 paged writer: 1024-byte binary payload, 50 sequential writes,
100 latest reads, 20 reopen-plus-snapshot samples, and eight async writers with
five writes each. Backend setup is outside the startup counters. Workload shape
and payload match; scheduling, OS caches, and conflict retries vary between runs.

| Workload | v1 p50/p95/p99 ms | Paged p50/p95/p99 ms |
| --- | --- | --- |
| Sequential durable write, 50 samples | 32.880 / 41.736 / 44.872 | 81.065 / 114.630 / 123.290 |
| Repeated latest read, 100 samples | 0.204 / 0.284 / 4.237 | 0.604 / 4.986 / 7.529 |
| Reopen plus snapshot, 20 samples | 0.392 / 0.490 / 0.523 | 0.969 / 20.939 / 34.598 |
| Concurrent durable write, 40 samples | 92.648 / 1972.636 / 2490.426 | 319.218 / 7080.338 / 9559.662 |

At 90 documents/90 retained requests, opening fetched 30,516 logical bytes for
v1 and 795 for paged metadata. Both read two heads and one immutable object;
v2's object is a 381-byte root read through a bounded range. It loads zero
location/request/membership pages, receipt objects, or document records on open.
Metadata range reads must not be confused with document-range reads.

Concurrent publication needed 153 CAS attempts/113 conflicts for v1 and
183 attempts/143 conflicts for v2, each acknowledging 40 commits. Total retained
objects, including history and orphan preparations, were 407 / 3,894,607 bytes
for v1 and 1,399 / 3,784,730 bytes for v2. Total logical written bytes including
successful HEAD updates were 3,913,080 and 3,803,567 respectively. These totals
include different retry amplification and are not a controlled storage-cost gain.

The paged layout performs more small object operations and was slower on this
small workload. Its benefit is bounded metadata opening and local updates as
state grows. It does not remove HEAD contention or retained-history growth.
Logical adapter operations/bytes are not physical HTTP requests or S3 cost.

A second paged baseline accounts for immutable uploads per publication attempt.
Each concurrent writer has its own benchmark-only wrapper and submits commits
sequentially; the wrapper attributes created bytes to that writer's next CAS.
It asserts that the outcome totals reconcile with global creates and conflicts.
Forty successful attempts created 240 objects / **802,620 bytes**, with another
8,280 bytes of successful HEAD writes. The 187 conflicting attempts abandoned
1,122 objects / **3,541,629 bytes**. No pending or unclassified uploads remained.
These are successful immutable-create bytes, including payload, receipt, pages
and root; transient editor allocations are not uploads. Preparation that ends
before CAS or has an uncertain outcome must remain pending/unclassified rather
than being inferred successful. This local run had neither case.

Concurrent p50/p95/p99 were 313.351 / 7332.392 / 8546.473 ms. It retained 1,663
objects / 4,759,067 bytes, including history and abandoned preparations. The
different conflict count reinforces why total bytes from separate contended runs
are not a controlled storage-efficiency comparison. Its full output follows the
scaling data in the linked measurement record.

### Capacity beyond the former manifest bound

The explicit release-mode POSIX capacity test passed with 100,000 live documents
and 800-byte padded names, inserted in batches of at most 256. Traversing the
committed location tree counted **91,200,000 live key/value bytes**, excluding
page envelopes and branch metadata, beyond the former 64 MiB manifest bound.
The root remained **381 bytes**; one update rewrote **five location pages** and
zero membership pages. Reopening, first/last point reads, a historical snapshot,
and the original receipt after later edits all passed. This run took 309.80 s.

```sh
cargo test -p obelisk-engine --release --locked \
  paged_capacity_exceeds_the_old_manifest_limit -- --ignored --nocapture
```

This test is deliberately ignored in routine CI because it creates a large
local disposable database. It is separate from the two credential-gated actual
AWS tests. Removing test-owned temporary folders is not database reclamation.

### Repeatable scaling workload

```sh
OBELISK_BASELINE_BACKEND=folder \
OBELISK_SCALING_MAX=100000 OBELISK_SCALING_REQUESTS=10000 \
cargo run --release -p obelisk --example scaling --locked
```

The workload measures 100, 1,000, 10,000, and 100,000 live documents with a
1024-byte payload and batches up to 256, then repeatedly updates one document
through increasing distinct request histories. It reports startup, point/bulk,
hierarchy, request resolution, and single-update counters; sampled read/fill/write
latencies; and retained object counts. Setup and probes are serial: contention
measurements are supplied by the small baseline and independent-process harness.

`obelisk::diagnostics::InstrumentedMetadata` distinguishes roots, each metadata
family, receipt objects, and document records without changing storage behavior.
Its reader decode charge is the root/page reader's bounded estimate, not RSS;
it excludes editor copies, result cloning and payload decoding. Generic counters
remain in `obelisk::storage::metrics`. Optional counters provide no correctness
or cache dependency. Both examples build with folder-only and S3-only features.

The full run reached all four live-document checkpoints and 10,000 retained
requests. [Recorded output](measurements/query-foundations-folder.txt) includes
every operation counter and latency distribution, with progress chatter removed.
The old writer is commit `d3e98aa`; paged measurements use this cycle's source.

| Live documents | Open bytes / tree reads | Point metadata bytes / tree reads | Descendants 100 metadata bytes / tree reads | Update objects / written bytes |
| ---: | --- | --- | --- | --- |
| 100 | 795 / 0 | 12,437 / 1 | 12,437 / 1 | 5 / 14,141 |
| 1,000 | 795 / 0 | 17,794 / 2 | 17,794 / 2 | 6 / 20,121 |
| 10,000 | 795 / 0 | 24,489 / 2 | 24,489 / 2 | 6 / 31,444 |
| 100,000 | 795 / 0 | 33,808 / 3 | 33,808 / 3 | 8 / 36,287 |

Every open reads one 381-byte root, zero tree pages/receipts/payloads, and has a
1,274-byte root reader charge. Metadata bytes above include HEAD and, for request
resolution, receipt objects. Each single update uses one successful CAS, has no
conflict or abandoned preparation, and creates zero membership pages. Written
bytes include all created immutable payload/receipt/page/root bytes and HEAD;
there is no separate WAL or hidden preparation upload. The baseline's contended
totals above include abandoned preparations and must be interpreted separately.

At 100,000 documents, bulk-100 and children-100 read 300 and 302 tree pages,
3,322,588 and 3,353,680 metadata bytes, and 111,600 payload bytes each. Children
enumerate the parent range, then perform separate location lookups. Bulk inputs
also perform separate point traversals: no shared immutable-page cache is present.
The descendant range reads three tree pages and the same 111,600 payload bytes.
This exposes a future batching/cache opportunity without hiding current costs.
The child-selectivity test separately proves that 3,072 deeper documents do not
make a one-child query scan their subtree.

| Live documents | Fill batch samples | Fill p50/p95/p99 ms | Open p50/p95/p99 ms | Descendants 100 p50/p95/p99 ms |
| ---: | ---: | --- | --- | --- |
| 100 | 1 | 90.347 / 90.347 / 90.347 | 13.494 / 20.262 / 20.262 | 15.576 / 24.690 / 24.690 |
| 1,000 | 4 | 246.393 / 600.455 / 600.455 | 0.410 / 6.017 / 6.017 | 11.140 / 26.343 / 26.343 |
| 10,000 | 36 | 190.148 / 315.081 / 321.137 | 9.695 / 20.010 / 20.010 | 9.279 / 12.893 / 12.893 |
| 100,000 | 352 | 244.008 / 620.580 / 990.232 | 0.573 / 5.788 / 5.788 | 14.432 / 37.267 / 37.267 |

Read distributions contain eight samples each; their p95/p99 are both the maximum
and do not characterize production tails. Fill batches contain at most 256
documents, with smaller checkpoint-ending batches. Retained live objects total
4,817 / 152,103,040 bytes, including prior roots/pages/receipts; 398 publications
all succeeded without conflicts. Logical bytes written including HEAD total
152,185,426. Ordinary engine reads made zero list/delete calls.

| Retained requests, one document | Open bytes / tree reads | Resolve original receipt metadata bytes / tree reads | Checkpoint write objects / written bytes | Last 32 writes p50/p95/p99 ms |
| ---: | --- | --- | --- | --- |
| 100 | 795 / 0 | 13,057 / 1 | 5 / 14,366 | 68.254 / 87.798 / 100.974 |
| 1,000 | 795 / 0 | 17,997 / 2 | 7 / 19,479 | 74.467 / 82.030 / 96.795 |
| 10,000 | 795 / 0 | 25,271 / 2 | 7 / 26,755 | 89.449 / 127.660 / 291.787 |

The hot document's point metadata stays at 745 bytes / one tree page while request
history grows; its encoded record is 1,103 bytes. Resolving `hot-1` still returns
the original receipt after all later writes. Retained hot objects total 65,649 /
243,050,831 bytes. Logical bytes written including HEAD total 245,121,038, with
10,001 successful publications (including initialization), zero conflicts and
zero list/delete calls. Paging bounds relevant metadata work; it does not bound
retained storage or guarantee flat write latency.

### Recovery and query visibility

The expanded subprocess suite checks four independent hierarchy writers through
36 committed outcomes: atomic parent pairs repeatedly appear/disappear while
leaves remain; pinned pages resume across concurrent deletion/recreation. Readers
compare membership results, document bodies and revisions from one snapshot.
Final outcomes and preserved historical pages are checked after writers exit.

Twenty-four commit-boundary kills cover receipt, document/request/membership page,
root and HEAD preparation/publication. Five use 256-document, maximum-path batches
that force multiple pages and tree splits. Markers must identify the intended
phase. After reopening, documents, children/descendants and retained outcomes are
visible together or absent together. Existing OS-lock-death and adapter-internal
rename-before-directory-sync tests remain required. Process kills do not establish
hardware power-loss durability.

The metadata-only rewrite qualification also replaces pages in all three trees
for a 256-document fixture, compares complete paginated results and revisions,
preserves original receipts and historical reads after reopen, and retries a stale
rewrite from fresh state. A response-loss test confirms explicit uncertainty after
successful metadata-only publication, without creating payload or receipt objects.
These tests exercise local POSIX storage; they add no performance measurements.

Actual AWS S3 remains pending: no dedicated bucket or test prefix is configured.
Neither emulator nor memory-adapter results count as actual-service qualification.

## MinIO HTTP transport smoke (2026-10-23)

The [emulator evidence log](measurements/minio-2026-10-23.txt) records S3-only
release probes through a dedicated local MinIO development container: 100/1,000
live documents and 100 retained requests. All opens transferred 795 logical bytes
without tree/payload/receipt reads. The companion storage contract and full
multiprocess hierarchy/crash suite passed. Image identity, prefixes, counters,
distributions and environment are recorded in the log; commands are in S3.md.
This small workload is not an actual AWS qualification or comparable latency
baseline (compilation/testing ran concurrently), and the development image has no
upstream release identity. Logical counters do not measure physical HTTP requests.


## Scalar lifecycle v3 qualification scope

Cycle 16B selects authoritative v3 and adds index lifecycle plus mandatory writer
maintenance. Existing tables above retain their original tested format and revision;
the v2 startup/root byte counts are historical evidence, not v3 measurements.
Cycle 16B runs correctness, bounded-work and root-only instrumentation tests plus
the required local checks. The full v3 release matrix (100 through 100,000 documents,
zero/one/multiple indexes and a building replacement), maximum-index safety,
large-capacity rerun and indexed process-kill suite remain cycle 18. No new latency
or storage-amplification baseline is claimed here.


## Indexed-query local work assertions (cycle 17)

Facade reference tests compare complete paginated full/projected results for exact
scalar queries. Logical counters prove key-only and empty projections perform no
cover/payload reads, covered binary/list selections perform no payload reads, and
full pages use metadata-only lookahead. A byte-limited uncovered-projection regression
first produced five payload reads for three results; it now reads each full payload
once by conservatively deferring the next record before fetching it. These are
correctness/work assertions on disposable POSIX folders, not release timings or
physical HTTP costs. The full v3 release matrix and capacity rerun remain cycle 18.


## Scalar release matrix methodology (cycle 18)

The `index_scaling` example uses one growing v3 database with 1,024 binary payload
bytes plus integer bucket/rank, boolean enabled and a string label. Seed batches
contain at most 256 documents. Each size measures zero indexes, one bucket/rank
index covering label, three active indexes, and those three with a building
replacement of the covering generation. The other indexes order rank and
(enabled, rank). Each initial index builds completely; replacement measurement
first commits 32 examined paths, pauses build advancement while serial/contended
foreground writes maintain the Building candidate, then finishes the replacement.
The timed foreground profiles include candidate maintenance but no simultaneous
builder IO; the separate process suite exercises concurrent builders and writers.
Build calls cap examined paths at 32 and source payload at 16 MiB. Root-only opening, eight-result selective key/covered/
full queries and exact path/rank expectations are asserted at each applicable phase.

Each profile includes eight key-changing serial document replacements and four
independent engine instances doing three replacements each. Writer-local upload
accounting classifies immutable preparations by subsequent CAS success/conflict,
reconciles those totals with adapter counters, and requires zero pending or
unclassified uploads. Index drops before the next size retain history. The dataset
and request history grow together; this is not a set of identical fresh databases.
Counters describe logical adapter operations and bytes including HEAD, not physical
HTTP requests or allocator RSS. OS caches are not flushed. Small sample percentile
summaries are observations, not universal targets or regression thresholds.

The complete matrix command is:

```sh
cargo run --release -p obelisk --example index_scaling --locked
```

The default folder run completed with exit zero through the 100,000-document
replacement activation, post-activation reference queries, drops and final retained
object counts. The [scalar qualification report](measurements/scalar-index-2026-10-31.md)
records the environment, exact engine/binary identity, raw output, latency/build
distributions, structural checks and successful/abandoned upload accounting.
The independent log audit verifies all 16 startup profiles, 16 upload profiles and
84 query rows. The populated v3 root is 386 bytes without a catalog and 467 with
one; opens transfer 800/881 logical bytes and load no other objects. Historical v2
byte counts above retain their original scope.

At 100,000 documents, eight selective results require 24 location-page and four
scalar-page reads, with zero full-record reads for key/covered projections and
exactly eight for full documents. Single-update structural tests preserve unrelated
child references. The separate release capacity gate passed in 193.67s with
91,200,000 live location key/value bytes and five location pages for one update.
The maximum 16-active-plus-candidate safety gate passed in release in 6.50s.

Costs remain material: the initial 100,000-document covering build took 49.08
minutes, read 8.49 billion logical bytes and wrote 1.38 billion. Serial update p50
at that size was 100.725/177.881/266.462/280.833ms for zero/one/three/replacement
profiles. The report includes all observations and small-sample distributions;
OS cache state, growing history and overlapping local tests limit comparisons.
No latency SLO, physical HTTP cost, hardware-power-loss or actual-AWS acceptance
claim follows. Retained storage and HEAD contention remain operational limits.
