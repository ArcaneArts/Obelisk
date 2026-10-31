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
The historical v1 manifests grew with document/request counts. Current v3
metadata extends the v2 immutable ordered pages with an index catalog; document segments remain sorted batches. No history replay, automatic compaction, or reclamation is required.

Default limits: 1 MiB encoded document, 1024 fields/document, 10000 list elements,
256 mutations and 256 explicit read conditions/batch, 16 MiB encoded transaction,
64 MiB metadata/object envelope, and 128 MiB total byte budget for a decoded object.
Public constructors and persistent decoders enforce bounded recursion and allocation.
V2 metadata page/root and aggregate operation budgets below replace the former
whole-manifest limit; legacy segment/value envelope bounds remain unchanged.
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

## Query foundation metadata primitives

Use separate immutable copy-on-write B+ trees for document locations, request
outcomes, and parent membership. The fixed-size root has three optional tree
references, identities, sequence, and a required membership capability. Empty trees
have no page. Document segments and transaction fingerprints retain their v1
encodings. The primitives cycle introduced these formats additively. Paged
integration now uses HEAD/root v2 and rejects v1 databases explicitly, without migration.

Pages target 16 KiB, have a 64 KiB encoded hard limit, and support at most 4096
entries and 32 levels (leaf level zero). Keys are at most 8704 bytes; leaf values
are at most 8192 bytes. Roots are at most 16 KiB. Original request receipts are
separate immutable objects bounded to 2 MiB, accommodating valid 256-mutation
receipts that exceed a page. Tree references include encoded length, hash, level,
and entry count. Readers validate family, checksums, ordering, separators and
strictly decreasing levels. Traversal uses root-based stacks, without leaf links.

Operation budgets start at 4096 metadata page reads, 64 MiB transferred metadata,
64 MiB cumulative decoded-allocation charges, and 64 MiB prepared page bytes.
Budgets are charged before reads/allocations; exhaustion never publishes HEAD.
These are work/safety bounds, not a global metadata-size limit. Batch editing
coalesces shared tree paths and uploads only newly reachable pages; deletion
rebalances siblings and collapses unary roots. No objects are reclaimed.

Ordered path keys concatenate UTF-8 segments with NUL terminators. Validated segments
cannot contain NUL. Membership keys insert an additional NUL before the final
segment, delimiting the parent scope (including virtual root). This preserves
structured path and `(parent, final segment)` order without interpreting escaped
textual delimiters as boundaries. Scalar index encodings are separate future work.

## Paged engine integration

Snapshots hold fixed-size tree roots and load pages only for targeted operations.
Latest reads still fetch authoritative HEAD. `Snapshot::revision` is async and
returns an owned optional revision. `EngineOptions.resources` supplies validated
operation budgets; nonzero values may lower the defaults but cannot exceed the
hard safety maxima. Reader budgets also cover document/receipt decoding, and
prepared records are checked against bounded readers before publication.

Successful request fingerprints and original receipts survive metadata-only
publications. Metadata-only work gets a fresh commit/sequence while preserving
document revisions and application outcomes. The common internal publisher checks
base identity/sequence and persists objects/root before HEAD CAS. Conflicting
metadata work must revalidate from a fresh snapshot rather than rebase stale roots.
Parent membership changes only when document existence changes; deleting a parent
preserves descendants and their entries. No hierarchy backfill is needed later.

## Bulk and hierarchy query API

`QueryScope` distinguishes virtual root from validated document paths. Bulk reads
preserve order/duplicates/missing entries, accept at most 256 paths, and use one
snapshot. String inputs are all validated before storage reads. A bounded
operation-local map deduplicates physical bulk lookups; returned clones are charged
against memory and response limits.

Hierarchy pages return path/document/revision entries in structured ascending path
order. Children use exact parent-membership ranges; descendants use location-tree
segment-prefix ranges. Neither includes the scope document or synthesized parents.
Pages default to 100 entries, cap at 256 and 16 MiB encoded document-entry bytes,
and may use a lower caller byte limit. Memory and metadata-work caps also apply.
Fetching is currently sequential (below the allowed eight-read concurrency cap).
One metadata-key lookahead determines continuation without fetching its payload.

Opaque native Rust cursors bind committed snapshot, scope, query kind, ordering
version, and last emitted key. There is no public decoder or server session.
Resumption seeks strictly after that key in the same retained snapshot, including
through another engine instance. Page sizes and byte budgets may change without
changing the logical query. Query/database/snapshot/version/position mismatches
are explicit cursor errors. Portable string tokens and snapshot expiration remain
outside this retained-history, transport-free block.

## Qualification instrumentation and local page work

Optional engine-owned diagnostics classify versioned object envelopes and families;
storage stays byte-oriented and independent of public document types. Reader decode
charges are estimates used for bounded root/page work, not allocator RSS. Counters
are observational and never participate in publication or cache correctness.

Rebalance only edited pages and immediate siblings. An unchanged sparse fringe
must not become background maintenance for an unrelated write. Qualification
reproduced four uploads for a first-leaf update because a distant sparse tail was
rewritten; the corrected path creates only the changed leaf/root, retaining every
unrelated child reference. Descriptor item counts are checked against encoded page
headers before entry-vector allocations, so corrupt counts cannot undercharge the
operation's decode budget. Encodings remain unchanged by these safeguards.

## Scalar index primitives

An `IndexDefinition` has a stable caller-visible identity, an exact children or
descendants scope, one to eight ordered components, and up to 32 optional covered
field paths. Components name a field path, one exact scalar type and ascending or
descending order. Covered paths are canonicalized; duplicate components, impossible
ancestor/descendant component pairs, and duplicate key/covered paths are rejected.
Definitions are bounded to 7 KiB before the engine envelope. Missing components,
out-of-scope documents and exact type mismatches produce no entry. Integer and
double remain distinct. Binary and list components are unsupported.

Scalar keys encode components first and use the structured document path as the
ascending deterministic tie-breaker. Boolean uses 0/1. Signed integers flip the
sign bit and use big endian. Finite doubles use the standard sign-aware sortable
bit transform after public negative-zero normalization. Strings use UTF-8 byte
order with NUL escaping and a two-byte terminator. Descending components complement
their encoded payload, including the string terminator. Type tags remain explicit.
The encoded scalar tuple is limited to 4096 bytes so the maximum ordered document
key still fits the existing 8704-byte metadata-key limit. Oversized keys fail; they
are never truncated or hashed.

Definition values use checksummed kind 7/version 1 envelopes. Metadata page family
4 is reserved for definitions and family 5 for scalar entries. The existing B+
tree supplies immutable point/range/edit behavior and family separation. This
additive primitives cycle does not add either family to the production root, alter
HEAD/root v2, create indexes, maintain entries, encode covered projections, or make
an index query-visible. Those behaviors require the later lifecycle and transaction
cycles, so existing databases and writes are unchanged.

## Scalar lifecycle primitives (16A)

Introduce the next lifecycle formats additively before selecting them in the
production engine. `metadata::lifecycle::RootV3` wraps the existing root data with
an optional catalog tree and a live-index count. HEAD/root v3 require membership
and scalar-maintenance capabilities. Production still uses v2 in 16A; 16B will
reject v2 before preparation and require fresh folders/prefixes for v3. No silent
migration, reset or reclamation is permitted.

Catalog leaves will reference checksummed lifecycle descriptors (at most 16 KiB),
not inline definitions plus progress. A descriptor identifies its database/index,
references an immutable kind-7 definition, and holds an active generation, a
candidate, or both. Active and candidate generation identities must differ. A
candidate pins a committed v3 base head and stores examined-count/last-source-path
progress and an optional bounded semantic failure. Definition scope and catalog
identity are cross-validated when their separate objects are available. Committed
origin of a snapshot reference remains the engine's responsibility in 16B.

Scalar-entry values carry generation, document location/revision and an optional
covered-projection reference. Covered projections are separate immutable objects
with generation/path/revision identity and a typed document subset. This supports
valid binary and list covered values beyond a metadata leaf without relaxing
leaf bounds. Projection objects are bounded to the document maximum plus explicit
path/identity/envelope overhead. Descriptor, entry and projection envelopes are
independently versioned; decoders consume an aggregate caller allocation budget.

Public generation handles, progress and work options remain persistence-independent.
Initial bounds are 16 live definitions and one candidate build per database;
build calls default to 100 source documents and 16 MiB source bytes, with at most
256 documents and lower validated options. These types are not wired into engine
execution until 16B.

Management fingerprints have an explicit versioned domain and bind operation,
definition/handle, generation and build limits. Tagged outcome v2 distinguishes
unchanged original document receipts from management receipts with no document
revisions. Original document v1 fingerprints/receipts and the legacy nonempty
document-receipt check remain unchanged. Management outcomes retain operation,
handle, publication identity, resulting state/progress and bounded failure.
Creating/rebuilding yields Building; advancing may yield Building, Active or
Failed; dropping yields Dropped. Namespace deduplication and wrong-kind resolution
will be enforced by lifecycle integration, not by these pure codecs alone.

## Scalar lifecycle integration (16B)

Production now uses v3 HEAD/root and tagged v2 outcomes. V1/v2 databases reject
before preparation or publication, with no migration or reset. Root-only opening
remains lazy; an indexed snapshot loads descriptors/definitions/entries only when
needed. Old document fingerprints, encodings, revisions and original receipts are
preserved. Inspection returns at most 16 catalog entries from one snapshot.
Configured lower live-index limits reject index operations/writes rather than
silently skip committed generations; ordinary document reads remain lazy.

Registration and rebuild pin a committed source snapshot. A chunk enumerates exact
children/descendant paths there, validates its saved source position, reads their
current documents, and publishes entries plus progress through the common HEAD
CAS. Every foreground write maintains active and building generations, including
post-registration paths. Thus processed source paths plus all paths mutated since
registration cover current eligible documents at source exhaustion. Activation
needs no unbounded catch-up or final full scan. Conflicting builders reload both
current values and persisted progress; stale handles cannot resurrect dropped or
recreated generations. Rebuild preserves the old active generation until activation.

Failed semantic builds retain original progress/tree and any active generation;
failed candidates are not maintained. Repair/rebuild and drop remain explicit.
An oversized old key in an incomplete build had no possible candidate entry, so
repair/removal is allowed; new eligible oversized keys fail atomically. Eligibility
checks all components before encoding so a later missing/type-mismatched field
still omits an entry even when an earlier value alone is oversized.

Management mutations use the common persisted namespace, domain-separated
fingerprints and original receipts. Deduplication precedes generation/state
conditions. All management operations resolve response loss, including a concealed
success followed by a conflict with a one-attempt retry budget. Resolution-read
failure preserves explicit uncertainty and the request identity. No permanent
writer, worker lease, cache, cleanup or independent index publication is introduced.

Covered projections are prepared under decoded and prepared-byte budgets before
any upload. Maintained entries verify old generation/location/coverage identity;
corrupt definitions/entries and impossible source positions fail without a new
publication. Metadata-only roots retain catalog references, document revisions and
original outcomes. Public indexed querying and covered-read validation remain
cycle 17; expanded independent-process/index-kill/scaling evidence remains cycle 18.


Prepared HEAD/root bytes must fit a fresh configured snapshot-reader budget before
any object upload. This applies to initialization and the shared publisher, so
preparation cannot create authoritative state that the same profile cannot reopen.
The operation's aggregate preparation budget remains separately enforced.


## Indexed queries (17)

`IndexedQuery` validates an exact children/descendants scope, ordered equality
prefix, optional range on the first remaining component, complete suffix ordering,
and canonical projection. Predicates use exact typed scalar values. Prefix field
order matters; equality component directions are immaterial. All suffix directions
must match. The signature includes every index key field: unused extra components
could omit otherwise matching documents or alter document-path ties. No implicit
scan, residual filter or in-memory sort is planned. Select the smallest compatible
active ID or require an explicit ID; missing/inactive/incompatible indexes return
an `IndexUnavailable` error with the required signature and reason.

Range endpoints describe semantic value order independently of index direction.
Composite prefixes and their exclusive byte successors implement included/excluded
bounds, including descending strings and empty ranges. Only the selected generation
in the pinned root is read. Entry generation/location/revision and projection
identity/field membership are checked. Full reads additionally compare the document's
key against the stored key. Key-only projections avoid projection and payload reads;
covered projections read only the selected immutable cover object as needed.
Uncovered selections read the document from that same snapshot. Types and absent
fields survive projection; binary/list values remain valid covered values.

Native scalar cursors bind a versioned canonical BLAKE3 query identity, snapshot,
index handle and last composite key. Identity includes canonical projection and
explicit selection, not page budgets. Resume validates the position and its
membership in the original entry tree. Reopen, writes, rebuild and drop do not
invalidate retained continuations. Hierarchy cursors remain distinct. There is
no portable decoder, expiration, transport or new persistent format in this cycle.

Query specs are bounded to 64 KiB and selections to 1024 fields. Pages reuse the
100/256-entry and 16 MiB limits plus aggregate budgets. Metadata-only lookahead
never fetches another full payload. For a byte-limited uncovered projection,
stop conservatively if the next full record might exceed remaining response space;
an empty page can still fetch that record to return its smaller projection.
No maximum-fill guarantee is made. This avoids redundant full reads without adding
per-field size metadata or another persistence format.

## Scalar qualification and operational bounds (18)

The complete local release matrix and independent-process recovery suite qualify
the implemented scalar block within its documented POSIX prototype scope. The
100,000-document matrix compares zero/one/three active indexes plus a maintained
building replacement; the separate maximum-index gate proves correctness at 16
active definitions plus one candidate, not throughput at that maximum. Keep these
limits until a separate measured change justifies raising them.

Opening remains root-only: populated v3 roots measure 386 bytes without a catalog
and 467 with one, transferring 800/881 logical bytes including two HEAD reads.
Selective queries avoid document scans and covered/key-only results avoid full
payload reads, but repeated location traversals and index construction still have
substantial cost. The first 100,000-document covering build took 49.08 minutes in
32-path chunks on the recorded host. Foreground maintenance and abandoned CAS
preparations increase work; immutable history/outcomes/orphans remain retained.
Do not infer physical HTTP costs or deployment latency targets from logical
counters and small samples. Full evidence is in BASELINE.md and its scalar report.

No production engine/storage behavior changes in the qualification milestone.
Further read batching/caching, compaction, backup/restore and reclamation require
separate Tasks preserving authoritative HEAD freshness and snapshot/outcome safety.
Actual AWS and hardware power loss remain unqualified; older emulator evidence
does not qualify these indexes. Use fresh v3 storage and preserve older data with
its matching binary.
