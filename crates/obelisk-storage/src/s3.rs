//! S3 conditional publication via object_store's ETagMatch implementation.
use crate::*;
use futures::TryStreamExt;
use object_store::{
    ObjectStore, PutMode, PutOptions, UpdateVersion,
    aws::{AmazonS3Builder, S3ConditionalPut},
    path::Path,
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct S3 {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}
impl S3 {
    /// Credentials/configuration remain in the builder or its external provider.
    /// Conditional writes are forced on; no unconditional fallback is permitted.
    pub fn new(builder: AmazonS3Builder, prefix: &str) -> Result<Self> {
        let prefix = prefix_path(prefix)?;
        let store = builder
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(map_error)?;
        Ok(Self {
            store: Arc::new(store),
            prefix,
        })
    }
    pub fn from_env(bucket: &str, prefix: &str) -> Result<Self> {
        Self::new(AmazonS3Builder::from_env().with_bucket_name(bucket), prefix)
    }
    fn head_path(&self) -> Path {
        self.prefix.child("HEAD")
    }
    fn object_path(&self, id: ObjectId) -> Path {
        self.prefix.child("objects").child(id.to_string())
    }
}
fn prefix_path(prefix: &str) -> Result<Path> {
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix.ends_with('/')
        || prefix
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(Error::Invalid("use a nonempty canonical S3 prefix".into()));
    }
    Path::parse(prefix).map_err(|e| Error::Invalid(e.to_string()))
}
fn map_error(error: object_store::Error) -> Error {
    match error {
        object_store::Error::NotFound { .. } => Error::NotFound,
        object_store::Error::AlreadyExists { .. }
        | object_store::Error::Precondition { .. }
        | object_store::Error::NotModified { .. } => Error::Conflict,
        object_store::Error::NotImplemented | object_store::Error::NotSupported { .. } => {
            Error::Unsupported(error.to_string())
        }
        other => Error::Transport(other.to_string()),
    }
}
async fn collect(result: object_store::GetResult, limit: usize) -> Result<Bytes> {
    let len = result
        .range
        .end
        .checked_sub(result.range.start)
        .ok_or_else(|| Error::Invalid("invalid response range".into()))?;
    if len > limit as u64 || result.meta.size > limit as u64 {
        return Err(Error::Invalid("object byte limit".into()));
    }
    let mut stream = result.into_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.try_next().await.map_err(map_error)? {
        if out.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Invalid("response byte limit".into()));
        }
        out.extend_from_slice(&chunk);
    }
    if out.len() as u64 != len {
        return Err(Error::Transport("truncated object response".into()));
    }
    Ok(out.into())
}
#[async_trait]
impl Storage for S3 {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes> {
        let path = self.object_path(id);
        if let Some(range) = range {
            let meta = self.store.head(&path).await.map_err(map_error)?;
            if meta.size > MAX_OBJECT_BYTES as u64 {
                return Err(Error::Invalid("object byte limit".into()));
            }
            validate_range(&range, meta.size)?;
            if range.is_empty() {
                return Ok(Bytes::new());
            }
            let expected = range.end - range.start;
            let result = self
                .store
                .get_opts(
                    &path,
                    object_store::GetOptions {
                        range: Some(range.into()),
                        ..Default::default()
                    },
                )
                .await
                .map_err(map_error)?;
            let bytes = collect(result, MAX_OBJECT_BYTES).await?;
            if bytes.len() as u64 != expected {
                return Err(Error::Transport("incorrect range response".into()));
            }
            Ok(bytes)
        } else {
            collect(
                self.store.get(&path).await.map_err(map_error)?,
                MAX_OBJECT_BYTES,
            )
            .await
        }
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()> {
        if bytes.len() > MAX_OBJECT_BYTES {
            return Err(Error::Invalid("object byte limit".into()));
        }
        self.store
            .put_opts(
                &self.object_path(id),
                bytes.into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
            .map_err(map_error)?;
        Ok(())
    }
    async fn read_head(&self) -> Result<Option<Head>> {
        let result = match self.store.get(&self.head_path()).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(map_error(e)),
        };
        let token = result
            .meta
            .e_tag
            .clone()
            .ok_or_else(|| Error::Unsupported("S3 head GET omitted ETag".into()))?;
        Ok(Some(Head {
            bytes: collect(result, MAX_HEAD_BYTES).await?,
            version: VersionToken(token),
        }))
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken> {
        if bytes.len() > MAX_HEAD_BYTES {
            return Err(Error::Invalid("head byte limit".into()));
        }
        let mode = match expected {
            None => PutMode::Create,
            Some(v) => PutMode::Update(UpdateVersion {
                e_tag: Some(v.0.clone()),
                version: None,
            }),
        };
        let result = self
            .store
            .put_opts(
                &self.head_path(),
                bytes.into(),
                PutOptions {
                    mode,
                    ..Default::default()
                },
            )
            .await;
        match result {
            Ok(result) => result
                .e_tag
                .map(VersionToken)
                .ok_or_else(|| Error::Uncertain("S3 PUT omitted ETag".into())),
            Err(e) => match map_error(e) {
                e @ (Error::Conflict | Error::Unsupported(_)) => Err(e),
                e => Err(Error::Uncertain(e.to_string())),
            },
        }
    }
    async fn list(&self) -> Result<Vec<ObjectId>> {
        let prefix = self.prefix.child("objects");
        let mut stream = self.store.list(Some(&prefix));
        let mut out = Vec::new();
        while let Some(meta) = stream.try_next().await.map_err(map_error)? {
            if let Some(name) = meta.location.filename()
                && let Ok(id) = ObjectId::parse(name)
                && meta.location == self.object_path(id)
            {
                out.push(id);
            }
        }
        out.sort();
        Ok(out)
    }
    async fn delete(&self, id: ObjectId) -> Result<()> {
        match self.store.delete(&self.object_path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(map_error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn adapter_logic_against_memory_store() {
        // Exercises the adapter, not S3's network/durability guarantees.
        let store = S3 {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix_path("test/db").unwrap(),
        };
        let id = ObjectId::random();
        store.create(id, "hello".into()).await.unwrap();
        assert!(matches!(
            store.create(id, "bad".into()).await,
            Err(Error::Conflict)
        ));
        assert_eq!(store.read(id, Some(1..3)).await.unwrap(), "el");
        assert!(store.read(id, Some(0..6)).await.is_err());
        assert!(store.read_head().await.unwrap().is_none());
        let v = store
            .compare_exchange_head(None, "first".into())
            .await
            .unwrap();
        store
            .compare_exchange_head(Some(&v), "second".into())
            .await
            .unwrap();
        assert!(matches!(
            store.compare_exchange_head(Some(&v), "stale".into()).await,
            Err(Error::Conflict)
        ));
        assert_eq!(store.read_head().await.unwrap().unwrap().bytes, "second");
        assert_eq!(store.list().await.unwrap(), vec![id]);
        store.delete(id).await.unwrap();
    }
    #[test]
    fn rejects_ambiguous_prefixes() {
        for p in ["", "/x", "x/", "x//y", "x/../y"] {
            assert!(prefix_path(p).is_err());
        }
    }
}
