# Baseline decisions

These decisions refine the adjustable defaults in PLAN.md. The baseline is a
prototype, with no persistent-format compatibility promised across format versions.
Unsupported versions fail explicitly; opening never silently migrates data.

## Public model

Paths are case-sensitive Unicode with no implicit Unicode normalization. Textual
document paths use `/`; textual field paths use `.`. Empty segments and control
characters are rejected. Backslash escapes only the delimiter and backslash.
Structured constructors accept literal delimiters in segments. Canonical display
escapes them. Document root is virtual. `.` and `..` are ordinary logical names,
never filesystem traversal. Paths are limited to 128 segments and 4096 UTF-8 bytes.

Integers are i64. Doubles are finite IEEE binary64; reject NaN and infinity and
normalize negative zero to positive zero. Integer and double remain distinct.
Lists declare their element type even when empty, support nesting, preserve order
and duplicates, and reject heterogeneous elements. Value nesting is limited to 32.
No null, map, timestamp, or decimal type is exposed.

Documents reject duplicate and ancestor/descendant terminal field paths. Patches
remove named terminal fields first, then set fields, validating the final document.
Conflicting or duplicate patch targets are rejected; a parent terminal can be
replaced by child fields through an explicit remove plus set. Patching an absent
document fails. Deleting an absent document succeeds when its condition permits.
Deleting a document never deletes descendants.

## Transactions and identity

A batch contains at most one mutation per document path and explicit read conditions
on any paths. Conditions evaluate against one starting snapshot. Failed conditions
are not committed. Revision tokens are unique commit identities; delete/recreate
cannot reuse revisions. Batches are atomic, and each successful publication has a
fresh commit UUID. Request IDs are caller-provided nonempty strings (up to 256 bytes).
Snapshot references are opaque values obtained from committed snapshots; callers
cannot turn an unpublished manifest into a public snapshot reference.
Successful request IDs, canonical operation fingerprints, and original receipts
are retained indefinitely for the prototype. Different payloads cannot reuse IDs.
Retries after head conflicts recompute all state-dependent work. Latest reads fetch
the authoritative head; pinned snapshots remain readable because nothing is reclaimed.

## Storage and format

Storage uses engine-generated UUID object IDs, never logical paths. Adapters own
process-safe atomicity and durability. Folder publication uses an OS file lock,
file sync, atomic rename, and directory sync. Readers also sync the head directory
under a shared publication lock before returning a visible head: a publisher may
have died after rename and before
sync. This allows uncertain request resolution to complete the missing barrier.
Folder roots are canonicalized once at open so later working-directory changes do
not redirect an existing adapter. The supported baseline durability target is a
local POSIX filesystem; Windows support is conditional on verified equivalent
publication and directory-flush behavior. Network filesystems are unverified.
S3 uses immutable create and conditional head replacement; no unconditional fallback.
Every head payload must contain a fresh commit identity to prevent ABA.

Use explicit bounded binary encodings with format-kind/version envelopes and BLAKE3
checksums. Keys encode segment boundaries. Metadata indexes document locations and
request outcomes separately from document payloads, so opening reads metadata only.
Initial manifests may grow with document/request counts; initial segments are sorted
immutable batches. No history replay, automatic compaction, or reclamation is required.

Default limits: 1 MiB encoded document, 1024 fields/document, 10000 list elements,
256 mutations and 256 explicit read conditions/batch, 16 MiB encoded transaction,
64 MiB metadata/object envelope, and 128 MiB total byte budget for a decoded object.
Public constructors and persistent decoders enforce bounded recursion and allocation.
Prepared metadata is validated against reader limits before publication so the
engine cannot commit a manifest that its own bounded reader cannot reopen.
These are initial safety bounds, not performance claims; expose configurable engine
retry budgets first and expand validated resource configuration later.

## Errors and dependencies

Types report validation errors; storage distinguishes missing objects, precondition
conflicts, invalid ranges, I/O/transport errors, and uncertain publication outcomes.
Engine errors distinguish validation, resource limits, corruption, unsupported formats, revision
conflicts, missing documents, request-ID reuse, retry exhaustion, and uncertain
publication. Uncertain outcomes carry the request identity for later resolution.

Use Tokio for async hosts, fs2 for OS locks, UUID v4 for object/commit identities,
BLAKE3 for integrity/fingerprints, and thiserror for typed errors. Evaluate
object_store's S3 conditional create/update implementation before adopting it.
Public types do not derive a persistent serialization format.
