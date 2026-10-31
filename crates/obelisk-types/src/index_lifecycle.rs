//! Public index identities, progress and bounded build options. No storage format.
use crate::{CommitId, CommitReceipt, DocumentPath, IndexDefinition, IndexId, Result, invalid};
use std::fmt;

pub const MAX_LIVE_INDEXES: usize = 16;
pub const MAX_INDEX_BUILDS: usize = 1;
pub const MAX_BUILD_DOCUMENTS: usize = 256;
pub const MAX_BUILD_SOURCE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexGeneration(CommitId);
impl IndexGeneration {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        Ok(Self(CommitId::new(value)?))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl fmt::Display for IndexGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexHandle {
    id: IndexId,
    generation: IndexGeneration,
}
impl IndexHandle {
    pub fn new(id: IndexId, generation: IndexGeneration) -> Self {
        Self { id, generation }
    }
    pub fn id(&self) -> &IndexId {
        &self.id
    }
    pub fn generation(&self) -> &IndexGeneration {
        &self.generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBuildOptions {
    documents: usize,
    source_bytes: usize,
}
impl Default for IndexBuildOptions {
    fn default() -> Self {
        Self {
            documents: 100,
            source_bytes: MAX_BUILD_SOURCE_BYTES,
        }
    }
}
impl IndexBuildOptions {
    pub fn new(documents: usize) -> Result<Self> {
        if documents == 0 || documents > MAX_BUILD_DOCUMENTS {
            return Err(invalid("index build document limit must be 1..=256"));
        }
        Ok(Self {
            documents,
            ..Self::default()
        })
    }
    pub fn with_source_byte_limit(self, source_bytes: usize) -> Result<Self> {
        if source_bytes == 0 || source_bytes > MAX_BUILD_SOURCE_BYTES {
            return Err(invalid("invalid index build source byte limit"));
        }
        Ok(Self {
            source_bytes,
            ..self
        })
    }
    pub fn documents(self) -> usize {
        self.documents
    }
    pub fn source_bytes(self) -> usize {
        self.source_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLimits {
    live_indexes: usize,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            live_indexes: MAX_LIVE_INDEXES,
        }
    }
}
impl IndexLimits {
    pub fn new(live_indexes: usize) -> Result<Self> {
        if live_indexes == 0 || live_indexes > MAX_LIVE_INDEXES {
            return Err(invalid("live index limit must be 1..=16"));
        }
        Ok(Self { live_indexes })
    }
    pub fn live_indexes(self) -> usize {
        self.live_indexes
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexBuildProgress {
    examined: u64,
    last_path: Option<DocumentPath>,
}
impl IndexBuildProgress {
    pub fn new(examined: u64, last_path: Option<DocumentPath>) -> Result<Self> {
        if (examined == 0) != last_path.is_none() {
            return Err(invalid("index build count and position disagree"));
        }
        Ok(Self {
            examined,
            last_path,
        })
    }
    pub fn examined(&self) -> u64 {
        self.examined
    }
    pub fn last_path(&self) -> Option<&DocumentPath> {
        self.last_path.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Building,
    Active,
    Failed,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexBuildFailure {
    /// The existing document's eligible scalar tuple exceeds the hard key bound.
    ScalarKeyTooLarge(DocumentPath),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOperation {
    Create,
    Advance,
    Rebuild,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexCommand {
    Create(IndexDefinition),
    Advance(IndexHandle, IndexBuildOptions),
    Rebuild(IndexHandle),
    Drop(IndexHandle),
}
impl IndexCommand {
    pub fn operation(&self) -> IndexOperation {
        match self {
            Self::Create(_) => IndexOperation::Create,
            Self::Advance(_, _) => IndexOperation::Advance,
            Self::Rebuild(_) => IndexOperation::Rebuild,
            Self::Drop(_) => IndexOperation::Drop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReceipt {
    pub publication: CommitReceipt,
    pub handle: IndexHandle,
    pub operation: IndexOperation,
    pub state: IndexState,
    pub progress: IndexBuildProgress,
    pub failure: Option<IndexBuildFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBuildStatus {
    pub handle: IndexHandle,
    pub state: IndexState,
    pub progress: IndexBuildProgress,
    pub failure: Option<IndexBuildFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    pub definition: IndexDefinition,
    pub active: Option<IndexHandle>,
    pub candidate: Option<IndexBuildStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn work_and_identity_limits_are_validated() {
        assert_eq!(IndexBuildOptions::default().documents(), 100);
        for n in [0, 257, usize::MAX] {
            assert!(IndexBuildOptions::new(n).is_err());
        }
        assert!(IndexBuildOptions::new(256).is_ok());
        for n in [0, MAX_BUILD_SOURCE_BYTES + 1, usize::MAX] {
            assert!(
                IndexBuildOptions::default()
                    .with_source_byte_limit(n)
                    .is_err()
            );
        }
        assert!(
            IndexBuildOptions::new(1)
                .unwrap()
                .with_source_byte_limit(1)
                .is_ok()
        );
        for n in [0, 17, usize::MAX] {
            assert!(IndexLimits::new(n).is_err());
        }
        assert_eq!(IndexLimits::default().live_indexes(), 16);
        assert!(IndexGeneration::new("").is_err());
        assert!(IndexGeneration::new("line\nbreak").is_err());
        assert!(IndexGeneration::new("x".repeat(257)).is_err());
        let path = DocumentPath::parse("people/a").unwrap();
        assert!(IndexBuildProgress::new(0, Some(path.clone())).is_err());
        assert!(IndexBuildProgress::new(1, None).is_err());
        assert_eq!(
            IndexBuildProgress::new(1, Some(path.clone()))
                .unwrap()
                .last_path(),
            Some(&path)
        );
    }
}
