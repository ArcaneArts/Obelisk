use crate::{Result, corrupt};
use obelisk_types::DocumentPath;

// Validated segments cannot contain NUL. A NUL terminates each segment; no
// segment-count prefix interferes with lexicographic structured-path ordering.
fn segments_key(segments: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for segment in segments {
        out.extend_from_slice(segment.as_bytes());
        out.push(0);
    }
    out
}
pub fn document_key(path: &DocumentPath) -> Vec<u8> {
    segments_key(path.segments())
}
pub(crate) fn document_key_len(path: &DocumentPath) -> usize {
    path.segments()
        .iter()
        .map(|segment| segment.len() + 1)
        .sum()
}
pub fn document_path(key: &[u8]) -> Result<DocumentPath> {
    if key.len() > super::MAX_KEY_BYTES || key.last() != Some(&0) {
        return Err(corrupt("invalid ordered path"));
    }
    let parts = key[..key.len() - 1]
        .split(|b| *b == 0)
        .map(|s| {
            std::str::from_utf8(s)
                .map(str::to_owned)
                .map_err(|_| corrupt("invalid ordered UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    DocumentPath::from_segments(parts).map_err(|e| corrupt(&e.to_string()))
}
/// Prefix of the exact parent scope, including a separate end-of-scope marker.
pub fn parent_prefix(parent: Option<&DocumentPath>) -> Vec<u8> {
    let mut out = parent.map(document_key).unwrap_or_default();
    out.push(0);
    out
}
pub fn membership_key(path: &DocumentPath) -> Vec<u8> {
    let segments = path.segments();
    let mut out = segments_key(&segments[..segments.len() - 1]);
    out.push(0);
    out.extend_from_slice(segments.last().unwrap().as_bytes());
    out.push(0);
    out
}
pub fn membership_path(key: &[u8]) -> Result<DocumentPath> {
    if key.len() > super::MAX_KEY_BYTES {
        return Err(corrupt("membership key limit"));
    }
    let parts: Vec<_> = key.split(|b| *b == 0).collect();
    if parts.len() < 3 || !parts[parts.len() - 1].is_empty() || !parts[parts.len() - 3].is_empty() {
        return Err(corrupt("invalid membership key"));
    }
    let mut document = Vec::with_capacity(parts.len() - 2);
    for part in parts[..parts.len() - 3]
        .iter()
        .chain(std::iter::once(&parts[parts.len() - 2]))
    {
        document.push(
            std::str::from_utf8(part)
                .map_err(|_| corrupt("invalid membership UTF-8"))?
                .to_owned(),
        );
    }
    DocumentPath::from_segments(document).map_err(|e| corrupt(&e.to_string()))
}
/// Exclusive upper bound for all byte keys starting with the supplied prefix.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(byte) = end.pop() {
        if byte != 255 {
            end.push(byte + 1);
            return Some(end);
        }
    }
    None
}
