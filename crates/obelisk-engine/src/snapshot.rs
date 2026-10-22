use crate::{
    Error, ResourceLimits, Result, corrupt, format,
    metadata::{self, Budget, Family, HeadRef, ObjectRef, Root},
};
use obelisk_storage::{ObjectId, Storage, VersionToken};
use obelisk_types::*;
use std::{sync::Arc, time::Duration};
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub max_attempts: usize,
    pub resources: ResourceLimits,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}
impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            max_attempts: 64,
            resources: ResourceLimits::default(),
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
        self.resources.validate()
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
/// Opaque reference obtained only from committed state; no arbitrary decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReference(pub(crate) HeadRef);
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub(crate) store: Arc<dyn Storage>,
    pub(crate) manifest: Root,
    pub(crate) version: VersionToken,
    pub(crate) reference: HeadRef,
    pub(crate) resources: ResourceLimits,
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
            let root = Root {
                database_id: DatabaseId::new(uuid::Uuid::new_v4().to_string())?,
                commit_id: CommitId::new(uuid::Uuid::new_v4().to_string())?,
                sequence: 0,
                documents: None,
                requests: None,
                membership: None,
            };
            let bytes = metadata::encode_root(&root)?;
            let mut budget = engine.options.resources.budget();
            budget.charge_decoded(bytes.len() * 2 + 512)?;
            metadata::charge(&mut budget.prepared, bytes.len(), "prepared root bytes")?;
            let id = ObjectId::random();
            let head = HeadRef {
                database_id: root.database_id,
                commit_id: root.commit_id,
                sequence: 0,
                root: ObjectRef::new(id, &bytes),
            };
            engine.store.create(id, bytes.into()).await?;
            match engine
                .store
                .compare_exchange_head(None, metadata::encode_head(&head)?.into())
                .await
            {
                Ok(_) => return Ok(engine),
                Err(obelisk_storage::Error::Conflict) => engine.options.backoff(attempt).await,
                Err(error) => {
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
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_with_budget(&mut self.options.resources.budget())
            .await
    }
    pub(crate) async fn snapshot_with_budget(&self, budget: &mut Budget) -> Result<Snapshot> {
        let head = self
            .store
            .read_head()
            .await?
            .ok_or_else(|| corrupt("database head disappeared"))?;
        let reference = metadata::decode_head(&head.bytes)?;
        self.load_snapshot(reference, head.version, budget).await
    }
    pub async fn snapshot_at(&self, reference: SnapshotReference) -> Result<Snapshot> {
        let mut budget = self.options.resources.budget();
        let current = self.snapshot_with_budget(&mut budget).await?;
        if reference.0.database_id != current.manifest.database_id {
            return Err(corrupt("snapshot belongs to another database"));
        }
        self.load_snapshot(reference.0, VersionToken(String::new()), &mut budget)
            .await
    }
    pub(crate) async fn load_snapshot(
        &self,
        reference: HeadRef,
        version: VersionToken,
        budget: &mut Budget,
    ) -> Result<Snapshot> {
        budget.charge_decoded(reference.root.len as usize * 2 + 512)?;
        let bytes = reference
            .root
            .read(self.store.as_ref(), metadata::MAX_ROOT_BYTES, budget)
            .await?;
        let manifest = metadata::decode_root(&bytes)?;
        if manifest.database_id != reference.database_id
            || manifest.commit_id != reference.commit_id
            || manifest.sequence != reference.sequence
        {
            return Err(corrupt("head/root identity mismatch"));
        }
        Ok(Snapshot {
            store: self.store.clone(),
            manifest,
            version,
            reference,
            resources: self.options.resources.clone(),
        })
    }
    pub async fn get(&self, path: &DocumentPath) -> Result<Option<VersionedDocument>> {
        let mut budget = self.options.resources.budget();
        let snapshot = self.snapshot_with_budget(&mut budget).await?;
        snapshot.get_with_budget(path, &mut budget).await
    }
    /// Shared internal publication path, including metadata-only changes.
    pub(crate) async fn publish_root(
        &self,
        base: &Snapshot,
        root: &Root,
        objects: Vec<(ObjectId, bytes::Bytes)>,
        budget: &mut Budget,
        publication_attempted: &mut bool,
    ) -> Result<()> {
        if root.database_id != base.manifest.database_id
            || root.sequence
                != base
                    .sequence()
                    .checked_add(1)
                    .ok_or_else(|| corrupt("commit sequence overflow"))?
            || root.commit_id == base.manifest.commit_id
        {
            return Err(corrupt("invalid prepared publication identity"));
        }
        let bytes = metadata::encode_root(root)?;
        metadata::charge(&mut budget.prepared, bytes.len(), "prepared root bytes")?;
        budget.charge_decoded(bytes.len() * 2 + 512)?;
        let object = ObjectId::random();
        let reference = HeadRef {
            database_id: root.database_id.clone(),
            commit_id: root.commit_id.clone(),
            sequence: root.sequence,
            root: ObjectRef::new(object, &bytes),
        };
        let head = metadata::encode_head(&reference)?;
        for (id, bytes) in objects {
            self.store.create(id, bytes).await?;
        }
        self.store.create(object, bytes.into()).await?;
        *publication_attempted = true;
        self.store
            .compare_exchange_head(Some(&base.version), head.into())
            .await?;
        Ok(())
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
    pub fn reference(&self) -> SnapshotReference {
        SnapshotReference(self.reference.clone())
    }
    pub fn storage_version(&self) -> &VersionToken {
        &self.version
    }
    pub async fn revision(&self, path: &DocumentPath) -> Result<Option<Revision>> {
        Ok(self
            .location(path, &mut self.resources.budget())
            .await?
            .map(|l| l.revision))
    }
    pub(crate) async fn location(
        &self,
        path: &DocumentPath,
        budget: &mut Budget,
    ) -> Result<Option<format::Location>> {
        metadata::get(
            self.store.as_ref(),
            Family::Documents,
            self.manifest.documents.as_ref(),
            &metadata::document_key(path),
            budget,
        )
        .await?
        .map(|bytes| metadata::decode_location(&bytes))
        .transpose()
    }
    pub async fn get(&self, path: &DocumentPath) -> Result<Option<VersionedDocument>> {
        self.get_with_budget(path, &mut self.resources.budget())
            .await
    }
    pub(crate) async fn get_with_budget(
        &self,
        path: &DocumentPath,
        budget: &mut Budget,
    ) -> Result<Option<VersionedDocument>> {
        let Some(location) = self.location(path, budget).await? else {
            return Ok(None);
        };
        self.read_location(path, location, budget).await
    }
    pub(crate) async fn read_location(
        &self,
        path: &DocumentPath,
        location: format::Location,
        budget: &mut Budget,
    ) -> Result<Option<VersionedDocument>> {
        budget.charge_decoded((location.range.end - location.range.start) as usize)?;
        let bytes = self
            .store
            .read(location.object, Some(location.range))
            .await?;
        let (stored_path, document) = format::decode_record_budget(&bytes, &mut budget.decoded)?;
        if &stored_path != path {
            return Err(corrupt("document location path mismatch"));
        }
        Ok(Some(VersionedDocument {
            document: document.ok_or_else(|| corrupt("live location points to tombstone"))?,
            revision: location.revision,
        }))
    }
    pub(crate) async fn outcome_ref(
        &self,
        id: &RequestId,
        budget: &mut Budget,
    ) -> Result<Option<metadata::OutcomeRef>> {
        metadata::get(
            self.store.as_ref(),
            Family::Requests,
            self.manifest.requests.as_ref(),
            id.as_str().as_bytes(),
            budget,
        )
        .await?
        .map(|bytes| metadata::OutcomeRef::decode(&bytes))
        .transpose()
    }
    pub(crate) async fn outcome(
        &self,
        id: &RequestId,
        reference: &metadata::OutcomeRef,
        budget: &mut Budget,
    ) -> Result<format::RequestRecord> {
        budget.charge_decoded(reference.blob.len as usize)?;
        let bytes = reference
            .blob
            .read(self.store.as_ref(), metadata::MAX_OUTCOME_BYTES, budget)
            .await?;
        let record = metadata::decode_outcome_budget(&bytes, &mut budget.decoded)?;
        if record.fingerprint != reference.fingerprint
            || record.receipt.database_id != self.manifest.database_id
            || &record.receipt.request_id != id
            || record.receipt.sequence > self.sequence()
        {
            return Err(corrupt("invalid persisted request identity"));
        }
        Ok(record)
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
}
