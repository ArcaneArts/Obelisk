# Implementation checklist

Each completed cycle is committed separately. See git history for exact commits.

- [x] 1. Workspace, crate boundaries, CI, and baseline decisions.
- [x] 2. Validated public data model.
- [ ] 3. Storage contract and folder adapter.
- [ ] 4. S3 adapter and opt-in real-service contract tests.
- [ ] 5. Persistent formats and metadata-only snapshot opening.
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
