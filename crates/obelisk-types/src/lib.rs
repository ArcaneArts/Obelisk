//! Validated public types, independent of persistent encoding.
mod document;
mod index;
mod index_lifecycle;
mod index_query;
mod path;
mod query;
mod transaction;
mod value;
pub use document::*;
pub use index::*;
pub use index_lifecycle::*;
pub use index_query::*;
pub use path::*;
pub use query::*;
pub use transaction::*;
pub use value::*;

pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_DEPTH: usize = 32;
pub const MAX_FIELDS: usize = 1024;
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_LIST_ITEMS: usize = 10_000;
pub const MAX_BATCH_ITEMS: usize = 256;
pub const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid input: {0}")]
pub struct ValidationError(pub String);
pub type Result<T> = std::result::Result<T, ValidationError>;
pub(crate) fn invalid(message: &str) -> ValidationError {
    ValidationError(message.into())
}
