//! Snapshot and transaction engine. Persistent formats belong to this crate.
pub mod format;
mod snapshot;
use obelisk_types::{DocumentPath, RequestId};
pub use snapshot::{Engine, EngineOptions, Snapshot};

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
    #[error("request ID used for a different transaction: {0}")]
    RequestIdReuse(RequestId),
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
