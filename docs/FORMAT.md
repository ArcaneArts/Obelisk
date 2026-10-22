# Prototype persistent formats

Fresh databases use v2 HEAD/root and paged metadata. The production engine rejects
v1 databases without migration. The v1 section below documents unchanged document
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
43-byte checksummed header. The engine writes only v2 databases. It rejects a v1
HEAD before preparing objects for an existing database; older v1 readers reject a v2 HEAD.

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
scalar-entry trees. Neither family is referenced by the production v2 root yet.

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
