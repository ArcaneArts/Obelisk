use crate::{
    Engine, Error, Result, Snapshot,
    format::{self, HeadRef, Location, RequestRecord},
};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::collections::BTreeMap;

fn check(snapshot: &Snapshot, path: &DocumentPath, condition: &Condition) -> Result<()> {
    let revision = snapshot.revision(path);
    let matches = match condition {
        Condition::Any => true,
        Condition::Absent => revision.is_none(),
        Condition::Revision(expected) => revision == Some(expected),
    };
    if matches {
        Ok(())
    } else {
        Err(Error::RevisionConflict(path.clone()))
    }
}
impl Engine {
    /// Atomically publishes deterministic mutations against one starting snapshot.
    /// Successful requests are deduplicated before evaluating revision conditions.
    pub async fn commit(&self, transaction: &Transaction) -> Result<CommitReceipt> {
        let fingerprint = format::transaction_fingerprint(transaction)?;
        let mut publication_attempted = false;
        for attempt in 0..self.options.max_attempts {
            let snapshot = self.snapshot().await.map_err(|e| {
                if publication_attempted {
                    uncertain(transaction, e.to_string())
                } else {
                    e
                }
            })?;
            if let Some(receipt) = existing(&snapshot, transaction, &fingerprint)? {
                return Ok(receipt);
            }
            for condition in transaction.conditions() {
                check(&snapshot, &condition.path, &condition.condition)?;
            }
            for mutation in transaction.mutations() {
                check(&snapshot, &mutation.path, &mutation.condition)?;
            }
            let mut records = BTreeMap::new();
            for mutation in transaction.mutations() {
                let document = match &mutation.kind {
                    MutationKind::Put(document) => Some(document.clone()),
                    MutationKind::Delete => None,
                    MutationKind::Patch(patch) => {
                        let previous = snapshot
                            .get(&mutation.path)
                            .await?
                            .ok_or_else(|| Error::MissingDocument(mutation.path.clone()))?;
                        Some(previous.document.apply(patch)?)
                    }
                };
                records.insert(mutation.path.clone(), document);
            }
            let (segment, ranges) = format::encode_segment(&records)?;
            if segment.len() > MAX_TRANSACTION_BYTES {
                return Err(ValidationError("prepared transaction byte limit".into()).into());
            }
            let segment_id = ObjectId::random();
            let commit_id = CommitId::new(uuid::Uuid::new_v4().to_string())?;
            let revision = Revision::new(commit_id.as_str())?;
            let mut manifest = snapshot.manifest.clone();
            manifest.sequence = manifest
                .sequence
                .checked_add(1)
                .ok_or_else(|| crate::corrupt("commit sequence overflow"))?;
            manifest.commit_id = commit_id.clone();
            manifest.latest_segment = Some(segment_id);
            let mut revisions = BTreeMap::new();
            for (path, document) in &records {
                if document.is_some() {
                    manifest.documents.insert(
                        path.clone(),
                        Location {
                            object: segment_id,
                            range: ranges[path].clone(),
                            revision: revision.clone(),
                        },
                    );
                    revisions.insert(path.clone(), Some(revision.clone()));
                } else {
                    manifest.documents.remove(path);
                    revisions.insert(path.clone(), None);
                }
            }
            let receipt = CommitReceipt {
                database_id: manifest.database_id.clone(),
                request_id: transaction.request_id().clone(),
                commit_id: commit_id.clone(),
                sequence: manifest.sequence,
                revisions,
            };
            manifest.requests.insert(
                transaction.request_id().clone(),
                RequestRecord {
                    fingerprint,
                    receipt: receipt.clone(),
                },
            );
            let manifest_bytes = format::encode_manifest(&manifest)?;
            let manifest_id = ObjectId::random();
            let head = HeadRef {
                database_id: manifest.database_id.clone(),
                commit_id,
                sequence: manifest.sequence,
                manifest: manifest_id,
                manifest_hash: *blake3::hash(&manifest_bytes).as_bytes(),
            };
            self.store.create(segment_id, segment.into()).await?;
            self.store
                .create(manifest_id, manifest_bytes.into())
                .await?;
            publication_attempted = true;
            match self
                .store
                .compare_exchange_head(Some(&snapshot.version), format::encode_head(&head)?.into())
                .await
            {
                Ok(_) => return Ok(receipt),
                Err(obelisk_storage::Error::Conflict) => self.options.backoff(attempt).await,
                Err(error) => {
                    if let Ok(snapshot) = self.snapshot().await
                        && let Some(receipt) = existing(&snapshot, transaction, &fingerprint)?
                    {
                        return Ok(receipt);
                    }
                    return Err(uncertain(transaction, error.to_string()));
                }
            }
        }
        let snapshot = self
            .snapshot()
            .await
            .map_err(|e| uncertain(transaction, e.to_string()))?;
        match existing(&snapshot, transaction, &fingerprint)? {
            Some(receipt) => Ok(receipt),
            None => Err(Error::RetryExhausted),
        }
    }
    /// None means this fresh snapshot has no recorded outcome. An in-flight
    /// request may still publish; retry only the same transaction/request ID.
    pub async fn resolve(&self, request_id: &RequestId) -> Result<Option<CommitReceipt>> {
        Ok(self
            .snapshot()
            .await?
            .manifest
            .requests
            .get(request_id)
            .map(|r| r.receipt.clone()))
    }
}
fn uncertain(transaction: &Transaction, reason: String) -> Error {
    Error::Uncertain {
        request_id: transaction.request_id().clone(),
        reason,
    }
}
fn existing(
    snapshot: &Snapshot,
    transaction: &Transaction,
    fingerprint: &[u8; 32],
) -> Result<Option<CommitReceipt>> {
    match snapshot.manifest.requests.get(transaction.request_id()) {
        None => Ok(None),
        Some(record) if &record.fingerprint == fingerprint => Ok(Some(record.receipt.clone())),
        Some(_) => Err(Error::RequestIdReuse(transaction.request_id().clone())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::EngineOptions;
    use std::sync::Arc;
    fn path(s: &str) -> DocumentPath {
        DocumentPath::parse(s).unwrap()
    }
    fn field(s: &str) -> FieldPath {
        FieldPath::parse(s).unwrap()
    }
    fn doc(v: i64) -> Document {
        Document::new([(field("value"), Value::Integer(v))]).unwrap()
    }
    fn tx(id: &str, mutations: Vec<Mutation>) -> Transaction {
        Transaction::new(RequestId::new(id).unwrap(), mutations, vec![]).unwrap()
    }
    fn put(p: &str, v: i64) -> Mutation {
        Mutation {
            path: path(p),
            condition: Condition::Any,
            kind: MutationKind::Put(doc(v)),
        }
    }
    async fn engine(dir: &std::path::Path) -> Engine {
        Engine::open(
            Arc::new(obelisk_storage::folder::Folder::open(dir).await.unwrap()),
            EngineOptions::default(),
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn atomic_batches_revisions_patches_and_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let before = db.snapshot().await.unwrap();
        let receipt = db
            .commit(&tx("first", vec![put("a", 1), put("a/b", 2)]))
            .await
            .unwrap();
        assert_eq!(receipt.sequence, 1);
        assert!(before.get(&path("a")).await.unwrap().is_none());
        let pinned = db.snapshot().await.unwrap();
        let revision = pinned.revision(&path("a")).unwrap().clone();
        let patch = Patch::new([(field("other"), Value::Boolean(true))], vec![]).unwrap();
        db.commit(&tx(
            "patch",
            vec![Mutation {
                path: path("a"),
                condition: Condition::Revision(revision.clone()),
                kind: MutationKind::Patch(patch),
            }],
        ))
        .await
        .unwrap();
        assert_eq!(
            db.get(&path("a"))
                .await
                .unwrap()
                .unwrap()
                .document
                .get(&field("value")),
            Some(&Value::Integer(1))
        );
        let stale = tx(
            "stale",
            vec![
                Mutation {
                    condition: Condition::Revision(revision),
                    ..put("a", 9)
                },
                put("never", 10),
            ],
        );
        assert!(matches!(
            db.commit(&stale).await,
            Err(Error::RevisionConflict(_))
        ));
        assert!(db.get(&path("never")).await.unwrap().is_none());
        db.commit(&tx(
            "delete",
            vec![Mutation {
                path: path("a"),
                condition: Condition::Any,
                kind: MutationKind::Delete,
            }],
        ))
        .await
        .unwrap();
        assert!(db.get(&path("a")).await.unwrap().is_none());
        assert!(db.get(&path("a/b")).await.unwrap().is_some());
        assert_eq!(
            pinned.get(&path("a")).await.unwrap().unwrap().document,
            doc(1)
        );
        let reopened = engine(dir.path()).await;
        assert_eq!(
            reopened.get(&path("a/b")).await.unwrap().unwrap().document,
            doc(2)
        );
        assert_eq!(
            reopened
                .snapshot_at(pinned.reference())
                .await
                .unwrap()
                .sequence(),
            1
        );
    }
    #[tokio::test]
    async fn absent_and_explicit_read_conditions() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let create = Mutation {
            condition: Condition::Absent,
            ..put("a", 1)
        };
        db.commit(&tx("create", vec![create.clone()]))
            .await
            .unwrap();
        assert!(matches!(
            db.commit(&tx("again", vec![create])).await,
            Err(Error::RevisionConflict(_))
        ));
        let transaction = Transaction::new(
            RequestId::new("read-condition").unwrap(),
            vec![put("b", 1)],
            vec![ReadCondition {
                path: path("a"),
                condition: Condition::Absent,
            }],
        )
        .unwrap();
        assert!(matches!(
            db.commit(&transaction).await,
            Err(Error::RevisionConflict(_))
        ));
        let missing_patch = tx(
            "missing",
            vec![Mutation {
                path: path("missing"),
                condition: Condition::Any,
                kind: MutationKind::Patch(Patch::new([], vec![]).unwrap()),
            }],
        );
        assert!(matches!(
            db.commit(&missing_patch).await,
            Err(Error::MissingDocument(_))
        ));
    }
    #[tokio::test]
    async fn concurrent_disjoint_writers_preserve_every_commit() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let mut tasks = Vec::new();
        for i in 0..12 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                db.commit(&tx(&format!("r{i}"), vec![put(&format!("docs/{i}"), i)]))
                    .await
                    .unwrap()
            }));
        }
        let mut sequences = std::collections::BTreeSet::new();
        for task in tasks {
            sequences.insert(task.await.unwrap().sequence);
        }
        assert_eq!(sequences.len(), 12);
        let snapshot = db.snapshot().await.unwrap();
        assert_eq!(snapshot.sequence(), 12);
        for i in 0..12 {
            assert_eq!(
                snapshot
                    .get(&path(&format!("docs/{i}")))
                    .await
                    .unwrap()
                    .unwrap()
                    .document,
                doc(i)
            );
        }
    }
    #[derive(Debug)]
    struct ResponseFault {
        inner: Arc<dyn obelisk_storage::Storage>,
        mode: std::sync::atomic::AtomicU8,
    }
    #[async_trait::async_trait]
    impl obelisk_storage::Storage for ResponseFault {
        async fn read(
            &self,
            id: ObjectId,
            range: Option<std::ops::Range<u64>>,
        ) -> obelisk_storage::Result<bytes::Bytes> {
            self.inner.read(id, range).await
        }
        async fn create(&self, id: ObjectId, bytes: bytes::Bytes) -> obelisk_storage::Result<()> {
            self.inner.create(id, bytes).await
        }
        async fn read_head(&self) -> obelisk_storage::Result<Option<obelisk_storage::Head>> {
            if self
                .mode
                .compare_exchange(
                    5,
                    0,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                return Err(obelisk_storage::Error::Transport(
                    "injected read failure".into(),
                ));
            }
            self.inner.read_head().await
        }
        async fn compare_exchange_head(
            &self,
            expected: Option<&obelisk_storage::VersionToken>,
            bytes: bytes::Bytes,
        ) -> obelisk_storage::Result<obelisk_storage::VersionToken> {
            let mode = self.mode.swap(0, std::sync::atomic::Ordering::SeqCst);
            if mode == 3 {
                return Err(obelisk_storage::Error::Uncertain(
                    "injected before publication".into(),
                ));
            }
            let result = self.inner.compare_exchange_head(expected, bytes).await?;
            match mode {
                1 => Err(obelisk_storage::Error::Uncertain(
                    "lost success response".into(),
                )),
                2 => Err(obelisk_storage::Error::Conflict),
                4 => {
                    self.mode.store(5, std::sync::atomic::Ordering::SeqCst);
                    Err(obelisk_storage::Error::Uncertain(
                        "lost success and resolution read".into(),
                    ))
                }
                _ => Ok(result),
            }
        }
        async fn list(&self) -> obelisk_storage::Result<Vec<ObjectId>> {
            self.inner.list().await
        }
        async fn delete(&self, id: ObjectId) -> obelisk_storage::Result<()> {
            self.inner.delete(id).await
        }
    }
    #[tokio::test]
    async fn request_records_survive_reopen_and_normalize_batch_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let transaction = tx("stable", vec![put("a", 1), put("b", 2)]);
        let receipt = db.commit(&transaction).await.unwrap();
        let reopened = engine(dir.path()).await;
        assert_eq!(
            reopened
                .commit(&tx("stable", vec![put("b", 2), put("a", 1)]))
                .await
                .unwrap(),
            receipt
        );
        assert_eq!(
            reopened.resolve(transaction.request_id()).await.unwrap(),
            Some(receipt)
        );
        assert!(matches!(
            reopened.commit(&tx("stable", vec![put("a", 9)])).await,
            Err(Error::RequestIdReuse(_))
        ));
        assert_eq!(reopened.snapshot().await.unwrap().sequence(), 1);
    }
    #[tokio::test]
    async fn concurrent_identical_requests_commit_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let transaction = tx(
            "same",
            vec![Mutation {
                condition: Condition::Absent,
                ..put("a", 1)
            }],
        );
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            let t = transaction.clone();
            tasks.push(tokio::spawn(async move { db.commit(&t).await.unwrap() }));
        }
        let expected = tasks.pop().unwrap().await.unwrap();
        for task in tasks {
            assert_eq!(task.await.unwrap(), expected);
        }
        assert_eq!(db.snapshot().await.unwrap().sequence(), 1);
    }
    #[tokio::test]
    async fn lost_response_and_transport_retry_conflict_resolve_original_receipt() {
        for mode in [1, 2, 3, 4] {
            let dir = tempfile::tempdir().unwrap();
            let fault = Arc::new(ResponseFault {
                inner: Arc::new(
                    obelisk_storage::folder::Folder::open(dir.path())
                        .await
                        .unwrap(),
                ),
                mode: std::sync::atomic::AtomicU8::new(0),
            });
            let db = Engine::open(
                fault.clone(),
                EngineOptions {
                    max_attempts: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            fault.mode.store(mode, std::sync::atomic::Ordering::SeqCst);
            let transaction = tx("uncertain", vec![put("a", 1)]);
            let result = db.commit(&transaction).await;
            if mode == 3 || mode == 4 {
                assert!(matches!(result, Err(Error::Uncertain { .. })));
            } else {
                assert!(result.is_ok());
            }
            let observed = db.resolve(transaction.request_id()).await.unwrap();
            assert_eq!(observed.is_some(), mode != 3);
            let receipt = db.commit(&transaction).await.unwrap();
            assert_eq!(receipt.sequence, 1);
            if let Some(original) = observed {
                assert_eq!(receipt, original);
            }
            assert_eq!(db.snapshot().await.unwrap().sequence(), 1);
        }
    }
}
