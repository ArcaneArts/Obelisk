# Implementation checklist

Each completed cycle is committed separately. See git history for exact commits.

- [x] 1. Workspace, crate boundaries, CI, and baseline decisions.
- [x] 2. Validated public data model.
- [x] 3. Storage contract and folder adapter.
- [x] 4. S3 adapter and opt-in real-service contract tests.
- [x] 5. Persistent formats and metadata-only snapshot opening.
- [ ] 6. Atomic transactions and conditional publication.
- [ ] 7. Persisted idempotency and uncertain-outcome resolution.
- [ ] 8. Public facade, examples, and usage documentation.
- [ ] 9. Multiprocess/crash validation and baseline measurements.

## Evidence

Cycle 1: workspace formatting, tests, lint, and no-default-feature build checked
before commit. The initial libraries intentionally only establish crate boundaries.

## Remaining external verification

Real S3 validation requires an explicitly configured isolated bucket prefix and
external credentials. CI does not silently substitute an emulator for real S3.
Power-loss durability, filesystem-specific guarantees, and platform support must
be distinguished from process-termination tests.

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
