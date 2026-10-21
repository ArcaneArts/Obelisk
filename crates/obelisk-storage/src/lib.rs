//! Byte-oriented contract. No document or transaction semantics live here.
use async_trait::async_trait;
use bytes::Bytes;
use std::ops::Range;

#[cfg(feature = "folder")]
pub mod folder;
#[cfg(feature = "s3")]
pub mod s3;

/// Maximum stored object size, including its engine envelope.
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024 + 64;
pub const MAX_HEAD_BYTES: usize = 4096;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("object not found")]
    NotFound,
    #[error("precondition failed")]
    Conflict,
    #[error("invalid storage input: {0}")]
    Invalid(String),
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage transport: {0}")]
    Transport(String),
    #[error("publication may have succeeded: {0}")]
    Uncertain(String),
    #[error("unsupported storage operation: {0}")]
    Unsupported(String),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(uuid::Uuid);
impl ObjectId {
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4())
    }
    pub fn parse(text: &str) -> Result<Self> {
        let id = uuid::Uuid::parse_str(text).map_err(|_| Error::Invalid("object ID".into()))?;
        if id.to_string() != text {
            return Err(Error::Invalid("noncanonical object ID".into()));
        }
        Ok(Self(id))
    }
}
impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
/// Opaque, adapter-specific token. Only the originating backend may interpret it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionToken(pub String);
#[derive(Debug, Clone)]
pub struct Head {
    pub bytes: Bytes,
    pub version: VersionToken,
}

/// All successful creates are durable and readable before returning. Immutable
/// creates never replace existing bytes. Head reads/CAS are linearizable.
/// `None` expects an absent head (race-safe database initialization).
/// Head payloads must include a unique commit identity on every publication.
/// Cancellation/transport failures may leave unpublished objects or an uncertain
/// head outcome; callers must resolve the transaction's persisted request record.
#[async_trait]
pub trait Storage: Send + Sync + std::fmt::Debug {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes>;
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()>;
    async fn read_head(&self) -> Result<Option<Head>>;
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken>;
    /// Maintenance only. Engine reads never discover data by listing objects.
    async fn list(&self) -> Result<Vec<ObjectId>>;
    /// Caller is responsible for proving the object is safe to reclaim.
    async fn delete(&self, id: ObjectId) -> Result<()>;
}
#[cfg(any(feature = "folder", feature = "s3"))]
pub(crate) fn validate_range(range: &Range<u64>, len: u64) -> Result<()> {
    if range.start > range.end || range.end > len {
        return Err(Error::Invalid("range outside object".into()));
    }
    Ok(())
}
