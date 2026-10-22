use crate::{Document, DocumentPath, Result, Revision, invalid};

pub const MAX_QUERY_ITEMS: usize = 256;
pub const MAX_QUERY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryScope {
    Root,
    Path(DocumentPath),
}
impl QueryScope {
    pub fn path(path: &str) -> Result<Self> {
        Ok(Self::Path(DocumentPath::parse(path)?))
    }
}
impl From<DocumentPath> for QueryScope {
    fn from(path: DocumentPath) -> Self {
        Self::Path(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageOptions {
    limit: usize,
    max_bytes: usize,
}
impl Default for PageOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            max_bytes: MAX_QUERY_BYTES,
        }
    }
}
impl PageOptions {
    pub fn new(limit: usize) -> Result<Self> {
        if limit == 0 || limit > MAX_QUERY_ITEMS {
            return Err(invalid("query limit must be 1..=256"));
        }
        Ok(Self {
            limit,
            ..Self::default()
        })
    }
    pub fn with_byte_limit(self, max_bytes: usize) -> Result<Self> {
        if max_bytes == 0 || max_bytes > MAX_QUERY_BYTES {
            return Err(invalid("invalid query byte limit"));
        }
        Ok(Self { max_bytes, ..self })
    }
    pub fn limit(self) -> usize {
        self.limit
    }
    pub fn max_bytes(self) -> usize {
        self.max_bytes
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct QueryEntry {
    pub path: DocumentPath,
    pub document: Document,
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_options_and_virtual_root_preserve_path_validation() {
        assert_eq!(PageOptions::default().limit(), 100);
        assert!(PageOptions::new(0).is_err());
        assert!(PageOptions::new(257).is_err());
        assert!(PageOptions::new(256).is_ok());
        assert!(PageOptions::default().with_byte_limit(0).is_err());
        assert!(
            PageOptions::default()
                .with_byte_limit(MAX_QUERY_BYTES + 1)
                .is_err()
        );
        assert!(QueryScope::path("").is_err());
        assert!(DocumentPath::parse("").is_err());
        assert_eq!(
            QueryScope::path("a/b").unwrap(),
            QueryScope::Path(DocumentPath::parse("a/b").unwrap())
        );
    }
}
