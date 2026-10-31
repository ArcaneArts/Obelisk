# Scalar-index qualification: 2026-10-31

The full folder-backend release matrix completed successfully, including final
replacement activation, post-activation reference queries, index drops and every
checkpoint at 100, 1,000, 10,000 and 100,000 documents. The process exited zero.
The independent log audit checked 16 root-only startup profiles, 16 reconciled
contention/upload profiles and 84 selective query rows. The raw output and audit
are retained alongside this report: [matrix output](scalar-index-2026-10-31-scaling.txt),
[audit result](scalar-index-2026-10-31-audit.json),
[audit script](scalar-index-matrix-audit.py) and
[required checks](scalar-index-2026-10-31-checks.txt).

## Tested implementation and environment

- Production engine/storage: `79b674ac73ff449fa367b362fdd1f29618ac9329`
  (cycle 17 merge). Cycle 18 adds qualification code and documentation without
  changing production semantics. Qualification source checkpoint:
  `3e56ce0e474d3d69f1196219e4891c5bfb3de67c`.
- The release executable was compiled from the cycle-18 harness working tree
  before its first source checkpoint. Its only subsequent harness-code change
  was the Clippy-required startup assertion loop rewrite from indexing to `zip`;
  the measured database operations and production code are unchanged.
- Executable SHA-256:
  `a691b00930a97832afcb18f4eea7ba881bf98b3199b23621eea53f84049e2fd8`.
- macOS 27.0.1 (26A434), arm64, Apple M4 Max, 128 GiB RAM.
  Rust `1.93.0 (254b59607)`, Cargo `1.93.0 (083ac5135)`.
- Local POSIX folder in a disposable `tempfile` directory. No engine cache;
  OS caches were not flushed. Local compilation/testing overlapped the run.
  Other host activity was uncontrolled. These are observed costs, not SLOs or a
  matched performance comparison with older baselines.

```sh
cargo run --release -p obelisk --example index_scaling --locked
python3 docs/measurements/scalar-index-matrix-audit.py \
  docs/measurements/scalar-index-2026-10-31-scaling.txt --require-complete
```

## Workload and interpretation

One database grows through four checkpoints. Documents have paths `docs/NNNNNN`,
1,024 binary payload bytes, integer bucket (`n % 1000`) and rank, a boolean enabled
field and a string label. Seed batches have at most 256 mutations. The covering
index is `(bucket ascending, rank descending)` with covered label; the other two
are rank ascending and `(enabled descending, rank ascending)`.

Each size measures zero, one and three active indexes, then three active indexes
plus one building replacement. Each profile has eight serial key-changing full
replacements and four independent engine instances performing three replacements
each. These timing writers are tasks in one process; the independent-process
correctness suite is separate. Replacement advancement pauses after 32 examined
paths during foreground timings; writers still maintain the candidate atomically.
Afterwards every remaining chunk runs, the candidate activates, and queries check
the activated generation. Thus these timing samples exclude simultaneous builder
IO; the coordinated process suite covers that race.

Build chunks examine at most 32 source paths and use a 16 MiB source-byte limit.
Initial rows include all chunks but exclude registration. Replacement rows cover
the remainder after its first 32-path chunk and exclude registration, that first
chunk and intervening foreground profiles. Every final progress count is the full
dataset size. Drops between sizes retain history and outcomes; these checkpoints
are not identical fresh databases. A final local temporary-directory cleanup is
test-fixture disposal, not database reclamation. S3 prefixes remain retained.

Queries select bucket zero, descending rank, limit eight, and compare exact paths
and ranks against the numeric reference model, including changed ranks. Projection
variants return key rank, key plus covered label, or full documents. At small sizes
there are fewer than eight matching documents. The independent facade suite covers
complete pagination, types, ranges, byte limits and retained cursors.

All counters are logical adapter operations/bytes. They do not measure physical
HTTP requests, transport retries, allocator RSS or disk-level write amplification.
Page counts include repeated traversals, not unique pages. Small-sample p95/p99
values often equal the maximum and do not estimate a deployment-wide tail.

## Bounded startup and selective reads

All 16 opens read exactly one root object and no catalog, scalar, location,
membership, request, definition, projection or document objects. HEAD is read twice.
Populated root sizes are 386 bytes without a catalog and 467 bytes with one;
startup transfers 800/881 logical bytes including HEAD. These are v3 measurements;
older v2 values in BASELINE.md retain their historical scope.

The following query samples use the single-index profile after its serial and
contended mutations. Every other applicable phase, including activated replacement,
also passed its exact reference and no-extra-payload assertions.

| Documents | Query | ms | Object reads | Read bytes incl HEAD | Location pages | Scalar pages | Full records | Projections |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | query-key | 4.309 | 7 | 29304 | 1 | 2 | 0 | 0 |
| 100 | query-covered | 4.243 | 8 | 29579 | 1 | 2 | 0 | 1 |
| 100 | query-full | 5.454 | 8 | 30605 | 1 | 2 | 1 | 0 |
| 1000 | query-key | 3.354 | 8 | 36824 | 2 | 2 | 0 | 0 |
| 1000 | query-covered | 4.637 | 9 | 37099 | 2 | 2 | 0 | 1 |
| 1000 | query-full | 4.108 | 9 | 38125 | 2 | 2 | 1 | 0 |
| 10000 | query-key | 5.670 | 23 | 223125 | 16 | 3 | 0 | 0 |
| 10000 | query-covered | 5.935 | 31 | 225325 | 16 | 3 | 0 | 8 |
| 10000 | query-full | 5.970 | 31 | 233533 | 16 | 3 | 8 | 0 |
| 100000 | query-key | 5.677 | 32 | 273584 | 24 | 4 | 0 | 0 |
| 100000 | query-covered | 6.682 | 40 | 275784 | 24 | 4 | 0 | 8 |
| 100000 | query-full | 6.032 | 40 | 283992 | 24 | 4 | 8 | 0 |

At 100,000 documents an eight-result query touches 24 location-page reads and four
scalar-page reads. Key-only and covered queries read zero full documents; covered
queries fetch eight small projection objects. Full queries read exactly eight
documents. Metadata traversal dominates the byte cost for this small payload.
There is no whole-document scan, object listing or extra full-document lookahead.

## Build work

Times are milliseconds. `Objects` and `Written bytes` count the measured chunks,
including successful HEAD writes. All build rows had zero CAS conflicts; concurrent
conflicts are measured separately below and forced in correctness tests.

| Documents | Index | Phase | Measured chunks | Final examined | ms | Object reads | Read bytes incl HEAD | Scalar pages created | Objects created | Written bytes incl HEAD |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | primary | initial | 4 | 100 | 1276.256 | 352 | 1900344 | 8 | 128 | 102435 |
| 100 | rank | initial | 4 | 100 | 346.247 | 328 | 2155825 | 8 | 28 | 95221 |
| 100 | enabled-rank | initial | 4 | 100 | 332.350 | 336 | 2194122 | 8 | 28 | 98745 |
| 100 | primary | replacement | 3 | 100 | 915.687 | 291 | 1538941 | 7 | 89 | 108939 |
| 1000 | primary | initial | 32 | 1000 | 14754.244 | 5421 | 30101807 | 81 | 1305 | 1515428 |
| 1000 | rank | initial | 32 | 1000 | 3758.614 | 5453 | 30407808 | 76 | 299 | 1325352 |
| 1000 | enabled-rank | initial | 32 | 1000 | 3940.867 | 5551 | 31619577 | 113 | 335 | 1781805 |
| 1000 | primary | replacement | 31 | 1000 | 13184.007 | 5438 | 30218518 | 80 | 1262 | 1768123 |
| 10000 | primary | initial | 313 | 10000 | 199023.181 | 62166 | 533779071 | 2567 | 14579 | 35360148 |
| 10000 | rank | initial | 313 | 10000 | 55391.843 | 55679 | 442325207 | 756 | 2715 | 14578878 |
| 10000 | enabled-rank | initial | 313 | 10000 | 73263.949 | 56965 | 455091080 | 1145 | 3382 | 20496662 |
| 10000 | primary | replacement | 312 | 10000 | 176858.464 | 63849 | 534481370 | 2570 | 14821 | 36447392 |
| 100000 | primary | initial | 3125 | 100000 | 2944868.047 | 906981 | 8491693071 | 139485 | 262482 | 1375333788 |
| 100000 | rank | initial | 3125 | 100000 | 567159.860 | 763049 | 5558939002 | 11274 | 34267 | 191944824 |
| 100000 | enabled-rank | initial | 3125 | 100000 | 521439.823 | 777554 | 5873346778 | 18826 | 41933 | 277730674 |
| 100000 | primary | replacement | 3124 | 100000 | 3034985.304 | 920008 | 8491725234 | 139245 | 262229 | 1370786351 |

The first 100,000-document covering build took 2,944,868.047 ms (49.08 minutes),
with 3,125 chunks, 906,981 object reads, 8,491,693,071 logical read bytes and
1,375,333,788 logical written bytes. Bounded invocation work does not imply a cheap
full build: repeated traversal, per-path preparation and retained immutable pages
remain substantial costs. Optimization requires a separate measured change.

## Foreground latency

All times are milliseconds. Serial profiles have eight observations; contended
profiles have twelve successful observations across four engine instances.
Profiles include durable publication and any retries.

| Documents | Profile | Operation | Samples | p50 ms | p95 ms | p99 ms |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | zero | foreground-update | 8 | 62.116 | 71.430 | 71.430 |
| 100 | zero | contended-update | 12 | 129.508 | 1240.037 | 1240.037 |
| 100 | one | foreground-update | 8 | 111.556 | 123.532 | 123.532 |
| 100 | one | contended-update | 12 | 301.705 | 2606.952 | 2606.952 |
| 100 | three | foreground-update | 8 | 176.339 | 196.143 | 196.143 |
| 100 | three | contended-update | 12 | 459.618 | 4282.365 | 4282.365 |
| 100 | three+replacement | foreground-update | 8 | 212.388 | 235.709 | 235.709 |
| 100 | three+replacement | contended-update | 12 | 456.973 | 4791.706 | 4791.706 |
| 1000 | zero | foreground-update | 8 | 90.711 | 101.133 | 101.133 |
| 1000 | zero | contended-update | 12 | 177.313 | 1666.973 | 1666.973 |
| 1000 | one | foreground-update | 8 | 136.207 | 156.078 | 156.078 |
| 1000 | one | contended-update | 12 | 378.674 | 3243.347 | 3243.347 |
| 1000 | three | foreground-update | 8 | 198.489 | 250.276 | 250.276 |
| 1000 | three | contended-update | 12 | 524.518 | 4756.035 | 4756.035 |
| 1000 | three+replacement | foreground-update | 8 | 221.277 | 235.329 | 235.329 |
| 1000 | three+replacement | contended-update | 12 | 676.967 | 5770.148 | 5770.148 |
| 10000 | zero | foreground-update | 8 | 78.203 | 101.370 | 101.370 |
| 10000 | zero | contended-update | 12 | 192.734 | 1676.111 | 1676.111 |
| 10000 | one | foreground-update | 8 | 255.223 | 272.805 | 272.805 |
| 10000 | one | contended-update | 12 | 391.593 | 3789.852 | 3789.852 |
| 10000 | three | foreground-update | 8 | 226.489 | 259.773 | 259.773 |
| 10000 | three | contended-update | 12 | 640.182 | 5290.527 | 5290.527 |
| 10000 | three+replacement | foreground-update | 8 | 243.678 | 382.411 | 382.411 |
| 10000 | three+replacement | contended-update | 12 | 720.293 | 6397.317 | 6397.317 |
| 100000 | zero | foreground-update | 8 | 100.725 | 109.223 | 109.223 |
| 100000 | zero | contended-update | 12 | 248.687 | 2514.820 | 2514.820 |
| 100000 | one | foreground-update | 8 | 177.881 | 191.734 | 191.734 |
| 100000 | one | contended-update | 12 | 515.816 | 4305.604 | 4305.604 |
| 100000 | three | foreground-update | 8 | 266.462 | 276.783 | 276.783 |
| 100000 | three | contended-update | 12 | 741.546 | 6234.348 | 6234.348 |
| 100000 | three+replacement | foreground-update | 8 | 280.833 | 297.324 | 297.324 |
| 100000 | three+replacement | contended-update | 12 | 876.338 | 6885.398 | 6885.398 |

At 100,000 documents, serial p50 rose from 100.725 ms without indexes to
177.881 ms with one, 266.462 ms with three, and 280.833 ms with three plus a
building replacement. This run demonstrates maintenance cost and single-HEAD
contention; no universal latency target or regression threshold is inferred.

## Single-update page and upload work

Each cell is the observed minimum–maximum over eight serial key-changing updates.
Unrelated scalar subtrees retain their exact references in a separate structural
test; these logical traversal counts alone would not prove that property.

| Documents | Profile | Object reads | Location pages read | Scalar pages read | Scalar pages created | Objects created | Written bytes incl HEAD |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | zero | 5 | 2 | 0 | 0 | 5 | 14128–14996 |
| 100 | one | 14 | 2 | 4 | 2 | 10 | 34557–35432 |
| 100 | three | 27 | 2 | 13 | 7 | 17 | 77283–78158 |
| 100 | three+replacement | 29 | 2 | 15 | 8 | 19 | 90664–91539 |
| 1000 | zero | 7 | 4 | 0 | 0 | 6 | 33361–34236 |
| 1000 | one | 18–19 | 4 | 4 | 2 | 13 | 58676–59397 |
| 1000 | three | 32–33 | 4 | 13–14 | 7 | 19 | 84080–85404 |
| 1000 | three+replacement | 34 | 4 | 15 | 8 | 21 | 93136–97520 |
| 10000 | zero | 9–10 | 4 | 0 | 0 | 7 | 37966–49550 |
| 10000 | one | 21 | 4 | 6 | 3 | 13 | 71666–72737 |
| 10000 | three | 35–36 | 4 | 14–15 | 7 | 20–21 | 115599–141979 |
| 10000 | three+replacement | 37 | 4 | 16 | 8 | 23 | 153525–154414 |
| 100000 | zero | 15 | 6 | 0 | 0 | 9 | 41098–42872 |
| 100000 | one | 28 | 6 | 8 | 4 | 16 | 98742–100507 |
| 100000 | three | 44–48 | 6 | 20–24 | 10 | 24 | 130999–153115 |
| 100000 | three+replacement | 46 | 6 | 22 | 11 | 26 | 162662–164930 |

## Successful and abandoned preparations

Each contention profile committed twelve operations. Every conflict corresponds
to one classified abandoned preparation; committed plus abandoned objects equal
the adapter object-create count. Their immutable bytes plus 2,484 successful HEAD
bytes (12 × 207) equal total bytes written. Pending and unclassified counts are
zero in every profile. Abandoned objects remain retained.

| Documents | Profile | Committed attempts | Committed objects | Committed bytes | Conflicts/abandoned attempts | Abandoned objects | Abandoned bytes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | zero | 12 | 60 | 187950 | 19 | 95 | 291004 |
| 100 | one | 12 | 120 | 433170 | 20 | 200 | 715433 |
| 100 | three | 12 | 210 | 978426 | 18 | 321 | 1494198 |
| 100 | three+replacement | 12 | 234 | 1138998 | 18 | 357 | 1735056 |
| 1000 | zero | 12 | 74 | 419110 | 17 | 102 | 587008 |
| 1000 | one | 12 | 156 | 716070 | 19 | 247 | 1129254 |
| 1000 | three | 12 | 240 | 1174757 | 18 | 359 | 1702734 |
| 1000 | three+replacement | 12 | 270 | 1406472 | 20 | 455 | 2388324 |
| 10000 | zero | 12 | 84 | 463217 | 19 | 133 | 719303 |
| 10000 | one | 12 | 156 | 859038 | 18 | 234 | 1281312 |
| 10000 | three | 12 | 245 | 1682212 | 21 | 424 | 2833099 |
| 10000 | three+replacement | 12 | 270 | 1843542 | 20 | 455 | 3116995 |
| 100000 | zero | 12 | 108 | 804894 | 21 | 189 | 1402017 |
| 100000 | one | 12 | 192 | 1235368 | 20 | 320 | 1996770 |
| 100000 | three | 12 | 299 | 1941977 | 19 | 473 | 2982615 |
| 100000 | three+replacement | 12 | 328 | 2257527 | 19 | 525 | 3600419 |

## Retained local objects

```text
retained size=100: objects=2357 bytes=9372169; includes history and abandoned preparations
retained size=1000: objects=8054 bytes=29065387; includes history and abandoned preparations
retained size=10000: objects=46545 bytes=167011261; includes history and abandoned preparations
retained size=100000: objects=655511 bytes=3561241698; includes history and abandoned preparations
```

Retained object counts include historical generations, outcomes, and abandoned
preparations. The engine made no list or delete calls. The benchmark's final local
directory enumeration counts files for evidence; ordinary engine paths do not use
listings for queries. Retained storage remains unbounded until a retention protocol
and reclamation are implemented.

## Companion qualification

- All six required checks and folder-only/S3-only example builds passed:
  **108 workspace tests passed, five ignored**. The ignores are two actual-AWS,
  two emulator and one opt-in capacity gate; the capacity gate ran separately.
- Release capacity passed in 193.67 seconds: 100,000 documents, 91,200,000 live
  location key/value bytes, 386-byte root, five location pages for one update,
  original receipts and retained reads preserved.
- Focused folder process suite passed in 106.03 seconds. Initial/rebuild phases
  each use four independent builders and four writers, compare full and covered
  results with pinned documents, and preserve atomic pairs. It also exercises a
  prepared stale builder against independent drop/recreate and retained cursors.
- All 50 new indexed kill markers, 24 existing publication markers and OS-lock
  death were reached and recovered. Adapter rename-before-directory-sync recovery
  remains a separate passing test. These kill client processes, not hardware.
- Maximum-index release gate passed in 6.50 seconds: 16 active definitions plus
  one candidate maintain an updated base path and an inserted post-base path;
  every active result and the activated replacement agree. A 17th definition
  rejects before object preparation/publication. This is a focused safety gate,
  not a 16-index throughput benchmark.
- A deterministic non-final chunk conflict recomputes current/unscanned/deleted/
  post-base paths, persists only the successful chunk's progress, and replays the
  original Building receipt after activation. Corruption/resource/reference and
  format-rejection coverage remain in the required workspace suite.

## Remaining acceptance limits

Actual AWS storage/recovery/scaling acceptance remains open: no dedicated external
bucket/test prefix was configured. No new emulator run is claimed. The older
MinIO evidence applies to its recorded revision and cannot qualify these indexes.
Windows folder storage is explicitly unsupported; platform CI does not establish
folder durability. Network filesystems and hardware power loss remain unverified.
Fresh v3 folders/prefixes are required. No migration/reset, compaction, reclamation,
backup/restore, portable cursor serialization or transport was introduced.

Raw matrix SHA-256: `1a1e98b6e5121580807598e106ce44137efa1a4807eb9ff9d7e02cabf1251eb0`.
