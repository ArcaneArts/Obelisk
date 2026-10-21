# Prototype format v1

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
