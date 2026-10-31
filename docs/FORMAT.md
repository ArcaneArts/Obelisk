# Prototype persistent formats

Fresh databases use v3 HEAD/root, a paged index catalog and maintained scalar
generations. The production engine rejects v1/v2 databases without migration. The v1 section below documents unchanged document
records/fingerprints and the former manifest layout for compatibility tests.

## Baseline format v1

All lengths/counts are unsigned u32 and all fixed integers are little endian.
An envelope is `OBLS` (4 bytes), kind (u8), version (u16 = 1), payload length
(u32), BLAKE3 payload digest (32 bytes), then exactly that payload. Its header is
43 bytes. Trailing bytes, unknown kinds/versions, bad lengths and checksums fail.
This is an internal prototype format, with no automatic migration.

Kinds: 1 head, 2 manifest, 3 segment, 4 document/tombstone record. A string or
binary sequence has a byte length followed by bytes. Strings are UTF-8. A path
has a segment count and length-prefixed segment strings; its boundaries do not
rely on textual separators. A format version covers these key and value encodings.

Values have tags 0 binary, 1 string, 2 i64, 3 finite f64 bits, 4 boolean (0/1),
5 typed list. Lists contain a recursive element-type tag, item count, and tagged
values. Type tags use the same numbers; a list type includes its nested type.
A document contains a field count and sorted (field-path, value) pairs.
A record contains a document path, presence byte, and an optional document.

Segments contain a count and sorted independently enveloped records. Manifest
locations give exact byte ranges for point reads; per-record checksums validate
range reads independently of the segment checksum. Tombstone records represent
explicit deletions. The authoritative complete manifest removes deleted paths,
so reads do not scan historical segments to determine absence.

Heads identify the database, commit, sequence, immutable manifest object, and hash
of the entire encoded manifest. Manifests repeat database/commit/sequence identity,
identify the latest segment, and index live document locations and revisions.
Request metadata stores each request ID, its canonical operation BLAKE3 digest,
and the full original receipt (database/request/commit identity, sequence and
per-document revisions). Requests have no expiration in the prototype. Fingerprints
normalize independent mutation and read-condition order while preserving value/list
order and explicit conditions.

Metadata is rewritten per commit in this baseline; startup reads the head and one
manifest, not history or document payloads. Metadata growth is deliberately bounded
at 64 MiB. Decoders enforce allocation, item-count, byte and nesting limits.

UUID object identifiers are physical keys, independent of logical paths. Revisions
and commit identities never depend on wall clocks. Folder HEAD additionally wraps
the engine head with a UUID storage-version token; S3 uses its opaque ETag.

## Paged metadata format v2

The primitives in `obelisk-engine::metadata` define root manifest kind 2/version 2,
page kind 5/version 1, and request-outcome kind 6/version 1. Envelopes keep the
43-byte checksummed header. Cycles 12–16A wrote v2 databases and rejected v1. Current production uses v3 as
described below; this historical codec remains for explicit compatibility tests.

A root repeats database/commit identity and sequence, requires capability mask 1
(parent membership maintained), then stores three optional page references in
family order: documents, requests, membership. It is bounded to 16 KiB.

An object reference contains a canonical UUID string, u32 encoded length, and
BLAKE3 hash of the complete encoded object. A page reference adds u8 level and
u32 item count. A page payload contains u8 family (1 documents, 2 requests,
3 membership), u8 level, u32 count, then strictly sorted length-prefixed byte
key/value pairs. Level zero values are record-family data; higher-level values
encode page references and keys give each child's exact minimum. Child levels
must be one lower. A child's maximum must precede the next separator. Page reads
use the referenced exact length and independently verify its hash and envelope.

Path keys contain segment UTF-8 bytes followed by NUL for every segment. Segment
NUL is prohibited by public validation. Membership keys concatenate the parent
path key, an additional NUL, and the final segment with its NUL terminator. A root
parent has only the additional NUL. Prefix successors give exclusive range ends.
These key encodings differ from the v1 length-prefixed round-trip path encoding.

An outcome object contains the canonical fingerprint followed by the original v1
receipt encoding. It is bounded to 2 MiB rather than to the 64 KiB page size.
A document-location leaf value contains the canonical segment UUID string,
u64 byte-range start/end, and revision string; ranges must identify a bounded live
record. Request leaf values contain the fingerprint and outcome object reference.
Membership leaf values are empty presence markers, with the path reconstructed
from the key. These trees are published atomically, including membership from the
first paged-format write.

HEAD kind 1/version 2 contains database/commit identity, sequence, and the root
object reference (including bounded length and integrity hash). Root identities
must agree with HEAD. The root's capability mask requires membership maintenance.
Only this small root is loaded on snapshot opening; all trees are lazy. Internal
metadata-only publications use the same CAS/durability path without changing
original document revisions or application receipts.

## Additive scalar-index primitives

Index-definition kind 7/version 1 encodes an index identity, children/descendants
scope and root/path scope value, one to eight ordered scalar component descriptors,
and zero to 32 canonical covered field paths. Paths retain explicit segment counts
and length-prefixed UTF-8 segments. Encoded definitions fit the 8192-byte metadata
leaf-value limit. Page family 4 identifies definition trees and family 5 identifies
scalar-entry trees. The historical v2 root does not reference either family.

A scalar-entry key concatenates every component encoding and then the existing
ordered document-path key as an ascending tie-breaker. Each component starts with
an exact type tag: 1 boolean, 2 integer, 3 double, 4 string. Booleans append 0/1.
Integers flip the i64 sign bit and append big-endian bytes. Doubles map normalized
finite IEEE bits by complementing negative encodings or flipping the sign bit for
nonnegative encodings, then append big-endian bytes. Strings append UTF-8 bytes,
escape NUL as `00 ff`, and terminate with `00 00`. A descending component bitwise
complements its payload but not its type tag. Decoders know component boundaries
from the definition and reject bad tags, escapes, UTF-8, booleans, bounds and paths.
Scalar tuples are at most 4096 bytes; complete keys retain the existing 8704-byte
limit. Prefix encodings and their exclusive byte successor support exact composite
prefix ranges without weakening document-path tie-breaking.

## Scalar lifecycle formats (v3; primitives 16A, integration 16B)

Production snapshots and transactions now select these codecs. HEAD/root v2 and
legacy outcome codecs remain explicit compatibility primitives, with no implicit
upgrade path. All new object kinds retain the checksummed 43-byte envelope.
Decoders validate exact length/version/kind and consume an explicit allocation
budget. Encoder self-validation uses the existing 128 MiB decoder safety maximum.
Neither encoding nor decoding publishes HEAD.

- HEAD kind 1/version 3 has the same identity/sequence/root-reference fields as
  v2, with an incompatible envelope version to prevent old writers opening it.
- Root kind 2/version 3 contains database/commit/sequence, required capability
  mask 3 (membership plus scalar maintenance), four optional page references in
  document/request/membership/catalog order, then a u32 live-index count. Counts
  are at most 16; zero count and absent catalog must agree. The root stays bounded
  to 16 KiB. Catalog count versus actual leaves and the single building-candidate bound are
  checked on catalog loading. Opening itself reads no catalog pages.
- Descriptor kind 8/version 1 is at most 16 KiB. Fields are database ID, index ID,
  definition object reference (kind 7 bound), optional active generation/root,
  and optional candidate. A generation/root is a bounded generation string and
  optional scalar page reference. A candidate additionally contains a
  length-prefixed v3 HEAD, progress and failure. At least one generation exists;
  active and candidate generations differ and base database identity agrees.
- Entry kind 9/version 1 fits the existing 8192-byte leaf-value limit. Fields
  are generation, length-prefixed existing document-location bytes and an optional
  covered-projection object reference. Location validation is unchanged.
- Projection kind 10/version 1 contains generation, structured document path,
  revision and the original typed document encoding. Its maximum is
  `MAX_DOCUMENT_BYTES + 2 * MAX_PATH_BYTES + 2048`. It contains only selected
  covered fields; absent fields remain absent. Definitions are cross-checked against catalog identity/scope. Maintenance checks
  entry generation, location/revision and projection presence. Query-time covered
  field and projection identity validation is part of cycle 17.

Progress encodes u64 examined count, a presence byte and optional structured
document path. Zero count is equivalent to an absent position. The source path
is interpreted using the definition's hierarchy scope, not a filesystem path.
Failure tag 0 means no failure; tag 1 carries the path of an oversized scalar key.

Outcome kind 6/version 2 starts with operation-kind tag (0 document, 1 index),
the 32-byte fingerprint and existing receipt fields. Document receipts retain
their nonempty revision map and original revision/commit identity checks. Index
receipts require an empty revision map, then encode index ID/generation, operation
(0 create, 1 advance, 2 rebuild, 3 drop), state (0 building, 1 active, 2 failed,
3 dropped), progress and failure. Publication sequence is nonzero. Failure is
present exactly for Failed, and operation/state combinations are validated.
Outcome objects retain the 2 MiB maximum. Shared-namespace lookup validates fingerprint, database, request identity and
committed sequence; resolve methods report explicit request-kind mismatches.

Management fingerprints hash the bytes `ObeliskIndexOperation-v1` plus NUL,
operation tag, and canonical arguments: create uses length-prefixed kind-7
definition bytes; other operations use index ID/generation; advance also includes
u32 document and source-byte limits. Request identity is excluded. Document
transaction fingerprints are unchanged.

## V3 publication and online build state

Index registration pins the current committed head in a candidate descriptor.
That base sequence must precede the descriptor's containing snapshot. Chunks
validate the base head/root identities, enumerate membership or location ranges,
validate any saved position against the pinned source tree, then read current
locations/documents from their publication snapshot. Source-byte limits stop
before an unprocessed path. Progress is persisted only with the matching candidate
pages; source exhaustion swaps the active root and removes the candidate in that
same CAS publication. A semantic oversized-key failure retains the previous chunk's
progress/tree and any active generation.

Foreground transactions read old eligible documents and prepare exact old-key
removals, current-value additions, covered projection objects, descriptor changes
and catalog pages. Changed location/revision data is updated even when scalar
components stay equal. All document/membership/index/outcome objects and the root
are durable before HEAD CAS. Conflict retries reopen authoritative state and
recompute these changes. Drop removes the current catalog entry/count, retaining
all old references. No new object family introduces automatic deletion.


## Native scalar-query cursor identity

Cycle 17 changes no persisted database encoding. Native Rust cursors have private
fields: committed snapshot reference, index ID/generation, ordering version 1,
last emitted composite key and a canonical query digest. The digest uses the
`ObeliskIndexedQuery-v1` domain, canonical scope/shape and scalar-value encodings,
range endpoint tags, sorted projection paths and explicit index selection. Page
limits are excluded. The engine validates the version, digest, database/snapshot,
selected generation, bounded position key, range/scope and original tree membership.
There is no public decoder or portable token contract. Historical v3 roots and
retained objects support continuations after current rebuild/drop without writing
cursor objects or server sessions.

## Qualified v3 sizes

The cycle-18 release matrix leaves these encodings unchanged. Populated roots
measured 386 bytes with the three original trees and no catalog, and 467 bytes
with a catalog reference. HEAD is 207 bytes, so an open that reads it twice and
the root once transfers 800/881 logical bytes. These fixture observations do not
replace the 16 KiB root bound. The capacity gate uses the actual 386-byte v3 root;
historical 381-byte v2 results remain labeled as older measurements. No format
upgrade, reclamation or cursor serialization is introduced by qualification.
