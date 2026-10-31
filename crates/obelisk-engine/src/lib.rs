//! Snapshot and transaction engine. Persistent formats belong to this crate.
pub mod diagnostics;
pub mod format;
mod index_build;
mod index_catalog;
mod index_lifecycle;
mod index_maintenance;
mod limits;
pub mod metadata;
mod query;
mod snapshot;
pub use limits::ResourceLimits;
pub use query::{QueryCursor, QueryPage};
mod transaction;
use obelisk_types::{DocumentPath, IndexHandle, IndexId, RequestId};
pub use snapshot::{Engine, EngineOptions, Snapshot, SnapshotReference};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Validation(#[from] obelisk_types::ValidationError),
    #[error(transparent)]
    Storage(#[from] obelisk_storage::Error),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u16),
    #[error("revision condition failed for {0}")]
    RevisionConflict(DocumentPath),
    #[error("document not found: {0}")]
    MissingDocument(DocumentPath),
    #[error("request ID used for a different operation: {0}")]
    RequestIdReuse(RequestId),
    #[error("request {0} belongs to another operation kind")]
    RequestKindMismatch(RequestId),
    #[error("index already exists: {0}")]
    IndexAlreadyExists(IndexId),
    #[error("index not found: {0}")]
    IndexNotFound(IndexId),
    #[error("index generation/state condition failed: {0:?}")]
    IndexStateConflict(IndexHandle),
    #[error("another index build is already in progress")]
    IndexBuildInProgress,
    #[error("invalid query cursor: {0}")]
    InvalidCursor(&'static str),
    #[error("resource limit: {0}")]
    ResourceLimit(&'static str),
    #[error("retry budget exhausted")]
    RetryExhausted,
    #[error(
        "commit outcome uncertain for request {request_id}: {reason}; resolve or retry the same transaction"
    )]
    Uncertain {
        request_id: RequestId,
        reason: String,
    },
}
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn corrupt(message: &str) -> Error {
    Error::Corrupt(message.into())
}

#[cfg(test)]
mod paged_tests;

#[cfg(test)]
mod qualification_tests;

#[cfg(test)]
mod index_tests;

#[cfg(test)]
mod index_race_tests;

#[cfg(test)]
mod index_corruption_tests;
