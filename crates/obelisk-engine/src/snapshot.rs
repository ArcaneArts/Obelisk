use crate::{
    Error, Result, corrupt,
    format::{self, HeadRef, Manifest},
};
use obelisk_storage::{ObjectId, Storage, VersionToken};
use obelisk_types::*;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub max_attempts: usize,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}
impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            max_attempts: 64,
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(50),
        }
    }
}
impl EngineOptions {
    pub fn validate(&self) -> Result<()> {
        if self.max_attempts == 0
            || self.max_attempts > 10_000
            || self.base_backoff > self.max_backoff
            || self.max_backoff > Duration::from_secs(60)
        {
            return Err(ValidationError("invalid retry options".into()).into());
        }
        Ok(())
    }
    pub(crate) async fn backoff(&self, attempt: usize) {
        let multiplier = 1_u32 << attempt.min(16);
        let delay = self
            .base_backoff
            .saturating_mul(multiplier)
            .min(self.max_backoff);
        tokio::time::sleep(delay).await;
    }
}
#[derive(Debug, Clone)]
pub struct Engine {
    pub(crate) store: Arc<dyn Storage>,
    pub(crate) options: EngineOptions,
}
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub(crate) store: Arc<dyn Storage>,
    pub(crate) manifest: Manifest,
    pub(crate) version: VersionToken,
    pub(crate) reference: HeadRef,
}
impl Engine {
    pub async fn open(store: Arc<dyn Storage>, options: EngineOptions) -> Result<Self> {
        options.validate()?;
        let engine = Self { store, options };
        for attempt in 0..engine.options.max_attempts {
            if engine.store.read_head().await?.is_some() {
                engine.snapshot().await?;
                return Ok(engine);
            }
            let manifest = Manifest {
                database_id: DatabaseId::new(uuid::Uuid::new_v4().to_string())?,
                commit_id: CommitId::new(uuid::Uuid::new_v4().to_string())?,
                sequence: 0,
                latest_segment: None,
                documents: BTreeMap::new(),
                requests: BTreeMap::new(),
            };
            let bytes = format::encode_manifest(&manifest)?;
            let id = ObjectId::random();
            engine.store.create(id, bytes.clone().into()).await?;
            let head = HeadRef {
                database_id: manifest.database_id.clone(),
                commit_id: manifest.commit_id.clone(),
                sequence: 0,
                manifest: id,
                manifest_hash: *blake3::hash(&bytes).as_bytes(),
            };
            match engine
                .store
                .compare_exchange_head(None, format::encode_head(&head)?.into())
                .await
            {
                Ok(_) => return Ok(engine),
                Err(obelisk_storage::Error::Conflict) => engine.options.backoff(attempt).await,
                Err(error) => {
                    // Initialization may have published before losing its response.
                    if engine.store.read_head().await?.is_some() {
                        engine.snapshot().await?;
                        return Ok(engine);
                    }
                    return Err(error.into());
                }
            }
        }
        Err(Error::RetryExhausted)
    }
    /// Fetches a fresh authoritative head; never silently returns a cached head.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let head = self
            .store
            .read_head()
            .await?
            .ok_or_else(|| corrupt("database head disappeared"))?;
        let reference = format::decode_head(&head.bytes)?;
        self.load_snapshot(reference, head.version).await
    }
    /// Opens a retained manifest by its immutable identity and integrity hash.
    pub async fn snapshot_at(&self, reference: HeadRef) -> Result<Snapshot> {
        let current = self.snapshot().await?;
        if reference.database_id != current.manifest.database_id {
            return Err(corrupt("snapshot belongs to another database"));
        }
        self.load_snapshot(reference, VersionToken(String::new()))
            .await
    }
    async fn load_snapshot(&self, reference: HeadRef, version: VersionToken) -> Result<Snapshot> {
        let bytes = self.store.read(reference.manifest, None).await?;
        if blake3::hash(&bytes).as_bytes() != &reference.manifest_hash {
            return Err(corrupt("manifest hash mismatch"));
        }
        let manifest = format::decode_manifest(&bytes)?;
        if manifest.database_id != reference.database_id
            || manifest.commit_id != reference.commit_id
            || manifest.sequence != reference.sequence
        {
            return Err(corrupt("head/manifest identity mismatch"));
        }
        Ok(Snapshot {
            store: self.store.clone(),
            manifest,
            version,
            reference,
        })
    }
    pub async fn get(&self, path: &DocumentPath) -> Result<Option<VersionedDocument>> {
        self.snapshot().await?.get(path).await
    }
}
impl Snapshot {
    pub fn database_id(&self) -> &DatabaseId {
        &self.manifest.database_id
    }
    pub fn commit_id(&self) -> &CommitId {
        &self.manifest.commit_id
    }
    pub fn sequence(&self) -> u64 {
        self.manifest.sequence
    }
    pub fn reference(&self) -> HeadRef {
        self.reference.clone()
    }
    pub fn revision(&self, path: &DocumentPath) -> Option<&Revision> {
        self.manifest.documents.get(path).map(|l| &l.revision)
    }
    pub async fn get(&self, path: &DocumentPath) -> Result<Option<VersionedDocument>> {
        let Some(location) = self.manifest.documents.get(path) else {
            return Ok(None);
        };
        let bytes = self
            .store
            .read(location.object, Some(location.range.clone()))
            .await?;
        let (stored_path, document) = format::decode_record(&bytes)?;
        if &stored_path != path {
            return Err(corrupt("document location path mismatch"));
        }
        let document = document.ok_or_else(|| corrupt("live location points to tombstone"))?;
        Ok(Some(VersionedDocument {
            document,
            revision: location.revision.clone(),
        }))
    }
    /// Storage token is exposed for engine-level diagnostics, not revision conditions.
    pub fn storage_version(&self) -> &VersionToken {
        &self.version
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn initialize_reopen_and_pinned_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            obelisk_storage::folder::Folder::open(dir.path())
                .await
                .unwrap(),
        );
        let engine = Engine::open(store.clone(), EngineOptions::default())
            .await
            .unwrap();
        let snapshot = engine.snapshot().await.unwrap();
        assert_eq!(snapshot.sequence(), 0);
        assert!(
            engine
                .get(&DocumentPath::parse("missing").unwrap())
                .await
                .unwrap()
                .is_none()
        );
        let reopened = Engine::open(store, EngineOptions::default()).await.unwrap();
        assert_eq!(
            reopened.snapshot().await.unwrap().database_id(),
            snapshot.database_id()
        );
        assert_eq!(
            reopened
                .snapshot_at(snapshot.reference())
                .await
                .unwrap()
                .commit_id(),
            snapshot.commit_id()
        );
    }
    #[tokio::test]
    async fn opening_reads_metadata_without_fetching_document_payloads() {
        use crate::format::Location;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            obelisk_storage::folder::Folder::open(dir.path())
                .await
                .unwrap(),
        );
        let engine = Engine::open(store.clone(), EngineOptions::default())
            .await
            .unwrap();
        let snapshot = engine.snapshot().await.unwrap();
        let path = DocumentPath::parse("a").unwrap();
        let (mut bytes, ranges) =
            format::encode_segment(&BTreeMap::from([(path.clone(), Some(Document::default()))]))
                .unwrap();
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        let object = ObjectId::random();
        store.create(object, bytes.into()).await.unwrap();
        let mut manifest = snapshot.manifest;
        manifest.documents.insert(
            path.clone(),
            Location {
                object,
                range: ranges[&path].clone(),
                revision: Revision::new("test").unwrap(),
            },
        );
        manifest.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        manifest.sequence = 1;
        manifest.latest_segment = Some(object);
        let bytes = format::encode_manifest(&manifest).unwrap();
        let manifest_id = ObjectId::random();
        store
            .create(manifest_id, bytes.clone().into())
            .await
            .unwrap();
        let head = HeadRef {
            database_id: manifest.database_id,
            commit_id: manifest.commit_id,
            sequence: 1,
            manifest: manifest_id,
            manifest_hash: *blake3::hash(&bytes).as_bytes(),
        };
        store
            .compare_exchange_head(
                Some(&snapshot.version),
                format::encode_head(&head).unwrap().into(),
            )
            .await
            .unwrap();
        let reopened = Engine::open(store, EngineOptions::default()).await.unwrap();
        assert!(matches!(reopened.get(&path).await, Err(Error::Corrupt(_))));
    }
}
