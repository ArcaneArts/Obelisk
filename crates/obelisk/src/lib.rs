//! Public application API for Obelisk.
//!
//! Latest reads fetch the authoritative head. Use a Snapshot for consistent
//! multi-document reads. Successful request IDs and outcomes are retained
//! indefinitely; retry uncertain writes with the same request ID and payload.
pub use obelisk_engine::diagnostics;
pub use obelisk_engine::{
    EngineOptions as DatabaseOptions, Error, IndexedQueryCursor, IndexedQueryPage, QueryCursor,
    QueryPage, ResourceLimits, Result, Snapshot, SnapshotReference,
};
pub use obelisk_storage as storage;
pub use obelisk_types::*;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Database {
    engine: obelisk_engine::Engine,
}
impl Database {
    pub async fn open_with_storage(
        store: Arc<dyn storage::Storage>,
        options: DatabaseOptions,
    ) -> Result<Self> {
        Ok(Self {
            engine: obelisk_engine::Engine::open(store, options).await?,
        })
    }
    #[cfg(feature = "folder")]
    pub async fn open_folder(path: impl Into<std::path::PathBuf>) -> Result<Self> {
        Self::open_with_storage(
            Arc::new(storage::folder::Folder::open(path).await?),
            DatabaseOptions::default(),
        )
        .await
    }
    #[cfg(feature = "s3")]
    pub async fn open_s3(bucket: &str, prefix: &str) -> Result<Self> {
        Self::open_with_storage(
            Arc::new(storage::s3::S3::from_env(bucket, prefix)?),
            DatabaseOptions::default(),
        )
        .await
    }
    pub async fn get(&self, path: &str) -> Result<Option<VersionedDocument>> {
        self.get_path(&DocumentPath::parse(path)?).await
    }
    pub async fn get_path(&self, path: &DocumentPath) -> Result<Option<VersionedDocument>> {
        self.engine.get(path).await
    }
    pub async fn get_many(&self, paths: &[&str]) -> Result<Vec<Option<VersionedDocument>>> {
        if paths.len() > MAX_QUERY_ITEMS {
            return Err(Error::ResourceLimit("bulk path count"));
        }
        let paths = paths
            .iter()
            .map(|p| DocumentPath::parse(p))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.get_many_paths(&paths).await
    }
    pub async fn get_many_paths(
        &self,
        paths: &[DocumentPath],
    ) -> Result<Vec<Option<VersionedDocument>>> {
        self.engine.get_many(paths).await
    }
    pub async fn children(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.engine.children(scope, options, cursor).await
    }
    pub async fn descendants(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.engine.descendants(scope, options, cursor).await
    }
    pub async fn query(
        &self,
        query: &IndexedQuery,
        options: PageOptions,
        cursor: Option<&IndexedQueryCursor>,
    ) -> Result<IndexedQueryPage> {
        self.engine.query(query, options, cursor).await
    }
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.engine.snapshot().await
    }
    pub async fn snapshot_at(&self, reference: SnapshotReference) -> Result<Snapshot> {
        self.engine.snapshot_at(reference).await
    }
    pub async fn commit(&self, transaction: &Transaction) -> Result<CommitReceipt> {
        self.engine.commit(transaction).await
    }
    pub async fn resolve(&self, request_id: &str) -> Result<Option<CommitReceipt>> {
        self.engine.resolve(&RequestId::new(request_id)?).await
    }
    pub async fn create_index(
        &self,
        request_id: &str,
        definition: IndexDefinition,
    ) -> Result<IndexReceipt> {
        self.engine
            .create_index(&RequestId::new(request_id)?, definition)
            .await
    }
    pub async fn get_index(&self, id: &IndexId) -> Result<Option<IndexStatus>> {
        self.engine.get_index(id).await
    }
    pub async fn list_indexes(&self) -> Result<Vec<IndexStatus>> {
        self.engine.list_indexes().await
    }
    pub async fn advance_index_build(
        &self,
        request_id: &str,
        handle: &IndexHandle,
        options: IndexBuildOptions,
    ) -> Result<IndexReceipt> {
        self.engine
            .advance_index_build(&RequestId::new(request_id)?, handle, options)
            .await
    }
    pub async fn rebuild_index(
        &self,
        request_id: &str,
        handle: &IndexHandle,
    ) -> Result<IndexReceipt> {
        self.engine
            .rebuild_index(&RequestId::new(request_id)?, handle)
            .await
    }
    pub async fn drop_index(&self, request_id: &str, handle: &IndexHandle) -> Result<IndexReceipt> {
        self.engine
            .drop_index(&RequestId::new(request_id)?, handle)
            .await
    }
    pub async fn resolve_index_operation(&self, request_id: &str) -> Result<Option<IndexReceipt>> {
        self.engine
            .resolve_index_operation(&RequestId::new(request_id)?)
            .await
    }
    pub async fn put(
        &self,
        request_id: &str,
        path: &str,
        document: Document,
        condition: Condition,
    ) -> Result<CommitReceipt> {
        self.mutate(request_id, path, MutationKind::Put(document), condition)
            .await
    }
    pub async fn create(
        &self,
        request_id: &str,
        path: &str,
        document: Document,
    ) -> Result<CommitReceipt> {
        self.put(request_id, path, document, Condition::Absent)
            .await
    }
    pub async fn patch(
        &self,
        request_id: &str,
        path: &str,
        patch: Patch,
        condition: Condition,
    ) -> Result<CommitReceipt> {
        self.mutate(request_id, path, MutationKind::Patch(patch), condition)
            .await
    }
    pub async fn delete(
        &self,
        request_id: &str,
        path: &str,
        condition: Condition,
    ) -> Result<CommitReceipt> {
        self.mutate(request_id, path, MutationKind::Delete, condition)
            .await
    }
    async fn mutate(
        &self,
        request_id: &str,
        path: &str,
        kind: MutationKind,
        condition: Condition,
    ) -> Result<CommitReceipt> {
        self.commit(&Transaction::new(
            RequestId::new(request_id)?,
            vec![Mutation {
                path: DocumentPath::parse(path)?,
                kind,
                condition,
            }],
            vec![],
        )?)
        .await
    }
}
