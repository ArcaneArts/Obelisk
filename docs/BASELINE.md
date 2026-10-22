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
