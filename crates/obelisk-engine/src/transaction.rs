use crate::{
    Engine, Error, Result, Snapshot, corrupt,
    format::{self, Location, RequestRecord},
    index_maintenance::{self, DocumentChanges},
    metadata::lifecycle::{self, TaggedOutcome},
    metadata::{self, Budget, Family, ObjectRef, OutcomeRef},
};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::collections::BTreeMap;

fn check(path: &DocumentPath, condition: &Condition, revision: Option<&Revision>) -> Result<()> {
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
    /// Deduplicate against committed outcomes before evaluating any conditions.
    pub async fn commit(&self, transaction: &Transaction) -> Result<CommitReceipt> {
        let fingerprint = format::transaction_fingerprint(transaction)?;
        let mut publication_attempted = false;
        for attempt in 0..self.options.max_attempts {
            let mut budget = self.options.resources.budget();
            let snapshot = self.snapshot_with_budget(&mut budget).await.map_err(|e| {
                if publication_attempted {
                    uncertain(transaction, e.to_string())
                } else {
                    e
                }
            })?;
            if let Some(receipt) = existing(&snapshot, transaction, &fingerprint, &mut budget)
                .await
                .map_err(|e| {
                    if publication_attempted && !matches!(e, Error::RequestIdReuse(_)) {
                        uncertain(transaction, e.to_string())
                    } else {
                        e
                    }
                })?
            {
                return Ok(receipt);
            }
            let indexes = snapshot.catalog(&mut budget).await?;
            for condition in transaction.conditions() {
                let location = snapshot.location(&condition.path, &mut budget).await?;
                check(
                    &condition.path,
                    &condition.condition,
                    location.as_ref().map(|l| &l.revision),
                )?;
            }
            let mut previous = BTreeMap::new();
            let mut before = BTreeMap::new();
            for mutation in transaction.mutations() {
                let location = snapshot.location(&mutation.path, &mut budget).await?;
                check(
                    &mutation.path,
                    &mutation.condition,
                    location.as_ref().map(|l| &l.revision),
                )?;
                budget.charge_decoded(512 + 3 * metadata::document_key_len(&mutation.path))?;
                if matches!(mutation.kind, MutationKind::Patch(_))
                    || indexes.iter().any(|index| {
                        index.maintained() && index.definition.scope().contains(&mutation.path)
                    })
                {
                    let document = if let Some(location) = location.as_ref() {
                        snapshot
                            .read_location(&mutation.path, location.clone(), &mut budget)
                            .await?
                    } else {
                        None
                    };
                    before.insert(mutation.path.clone(), document);
                }
                previous.insert(mutation.path.clone(), location);
            }
            let mut records = BTreeMap::new();
            let mut prepared_size = 4 + format::HEADER_BYTES;
            for mutation in transaction.mutations() {
                let document = match &mutation.kind {
                    MutationKind::Put(document) => {
                        budget.charge_decoded(format::document_allocation(document))?;
                        Some(document.clone())
                    }
                    MutationKind::Delete => None,
                    MutationKind::Patch(patch) => {
                        let previous = before
                            .get(&mutation.path)
                            .and_then(Option::as_ref)
                            .ok_or_else(|| Error::MissingDocument(mutation.path.clone()))?;
                        budget.charge_decoded(
                            format::document_allocation(&previous.document)
                                + format::fields_allocation(patch.set()),
                        )?;
                        Some(previous.document.apply(patch)?)
                    }
                };
                prepared_size += format::HEADER_BYTES
                    + 5
                    + mutation
                        .path
                        .segments()
                        .iter()
                        .map(|p| p.len() + 4)
                        .sum::<usize>()
                    + document.as_ref().map_or(Ok(0), Document::estimated_size)?;
                if prepared_size > MAX_TRANSACTION_BYTES {
                    return Err(Error::ResourceLimit("prepared transaction size"));
                }
                records.insert(mutation.path.clone(), document);
            }
            budget.charge_decoded(prepared_size)?;
            let (segment, ranges) = format::encode_segment(&records)?;
            if segment.len() > MAX_TRANSACTION_BYTES {
                return Err(Error::ResourceLimit("prepared transaction bytes"));
            }
            // Validate newly prepared records against this operation's reader budget.
            for range in ranges.values() {
                format::decode_record_budget(
                    &segment[range.start as usize..range.end as usize],
                    &mut budget.decoded,
                )?;
            }
            let segment_id = ObjectId::random();
            let commit_id = CommitId::new(uuid::Uuid::new_v4().to_string())?;
            let revision = Revision::new(commit_id.as_str())?;
            let mut root = snapshot.manifest.clone();
            root.sequence = root
                .sequence
                .checked_add(1)
                .ok_or_else(|| corrupt("commit sequence overflow"))?;
            root.commit_id = commit_id.clone();
            let mut documents = metadata::Changes::new();
            let mut membership = metadata::Changes::new();
            let mut revisions = BTreeMap::new();
            let mut locations = BTreeMap::new();
            for (path, document) in &records {
                let value = if document.is_some() {
                    let location = Location {
                        object: segment_id,
                        range: ranges[path].clone(),
                        revision: revision.clone(),
                    };
                    budget.charge_decoded(512 + metadata::document_key_len(path))?;
                    let value = metadata::encode_location(&location)?;
                    locations.insert(path.clone(), location);
                    Some(value)
                } else {
                    None
                };
                documents.insert(metadata::document_key(path), value);
                if previous[path].is_some() != document.is_some() {
                    membership.insert(
                        metadata::membership_key(path),
                        document.as_ref().map(|_| Vec::new()),
                    );
                }
                revisions.insert(path.clone(), document.as_ref().map(|_| revision.clone()));
            }
            let receipt = CommitReceipt {
                database_id: root.database_id.clone(),
                request_id: transaction.request_id().clone(),
                commit_id,
                sequence: root.sequence,
                revisions,
            };
            let receipt_memory = 1024
                + receipt
                    .revisions
                    .keys()
                    .map(|path| {
                        1024 + path.segments().len() * 64
                            + path.segments().iter().map(String::len).sum::<usize>()
                    })
                    .sum::<usize>();
            budget.charge_decoded(receipt_memory * 3)?;
            let outcome =
                lifecycle::encode_tagged_outcome(&TaggedOutcome::Document(RequestRecord {
                    fingerprint,
                    receipt: receipt.clone(),
                }))?;
            budget.charge_decoded(outcome.len() * 2)?;
            metadata::charge(
                &mut budget.prepared,
                outcome.len(),
                "prepared receipt bytes",
            )?;
            let outcome_id = ObjectId::random();
            let outcome_ref = OutcomeRef {
                fingerprint,
                blob: ObjectRef::new(outcome_id, &outcome),
            };
            let requests = BTreeMap::from([(
                transaction.request_id().as_str().as_bytes().to_vec(),
                Some(outcome_ref.encode()?),
            )]);
            let docs = metadata::prepare(
                self.store.as_ref(),
                Family::Documents,
                root.documents.as_ref(),
                &documents,
                &mut budget,
            )
            .await?;
            let members = metadata::prepare(
                self.store.as_ref(),
                Family::Membership,
                root.membership.as_ref(),
                &membership,
                &mut budget,
            )
            .await?;
            let requests = metadata::prepare(
                self.store.as_ref(),
                Family::Requests,
                root.requests.as_ref(),
                &requests,
                &mut budget,
            )
            .await?;
            root.documents = docs.root;
            root.membership = members.root;
            root.requests = requests.root;
            let mut objects = vec![(segment_id, segment.into()), (outcome_id, outcome.into())];
            objects.extend(docs.objects);
            objects.extend(members.objects);
            objects.extend(requests.objects);
            index_maintenance::maintain(
                &snapshot,
                &mut root,
                indexes,
                DocumentChanges {
                    before: &before,
                    previous_locations: &previous,
                    after: &records,
                    locations: &locations,
                },
                &mut objects,
                &mut budget,
            )
            .await?;
            match self
                .publish_root(
                    &snapshot,
                    &root,
                    objects,
                    &mut budget,
                    &mut publication_attempted,
                )
                .await
            {
                Ok(()) => return Ok(receipt),
                Err(Error::Storage(obelisk_storage::Error::Conflict)) => {
                    self.options.backoff(attempt).await
                }
                Err(error) => {
                    if !publication_attempted {
                        return Err(error);
                    }
                    let mut resolve_budget = self.options.resources.budget();
                    if let Ok(snapshot) = self.snapshot_with_budget(&mut resolve_budget).await
                        && let Ok(Some(receipt)) =
                            existing(&snapshot, transaction, &fingerprint, &mut resolve_budget)
                                .await
                    {
                        return Ok(receipt);
                    }
                    return Err(uncertain(transaction, error.to_string()));
                }
            }
        }
        let mut budget = self.options.resources.budget();
        let snapshot = self
            .snapshot_with_budget(&mut budget)
            .await
            .map_err(|e| uncertain(transaction, e.to_string()))?;
        existing(&snapshot, transaction, &fingerprint, &mut budget)
            .await
            .map_err(|e| {
                if matches!(e, Error::RequestIdReuse(_)) {
                    e
                } else {
                    uncertain(transaction, e.to_string())
                }
            })?
            .ok_or(Error::RetryExhausted)
    }
    /// None does not rule out an in-flight request. Retry only the same operation.
    pub async fn resolve(&self, request_id: &RequestId) -> Result<Option<CommitReceipt>> {
        let mut budget = self.options.resources.budget();
        let snapshot = self.snapshot_with_budget(&mut budget).await?;
        let Some(reference) = snapshot.outcome_ref(request_id, &mut budget).await? else {
            return Ok(None);
        };
        Ok(Some(
            snapshot
                .outcome(request_id, &reference, &mut budget)
                .await?
                .receipt,
        ))
    }
}
fn uncertain(transaction: &Transaction, reason: String) -> Error {
    Error::Uncertain {
        request_id: transaction.request_id().clone(),
        reason,
    }
}
async fn existing(
    snapshot: &Snapshot,
    transaction: &Transaction,
    fingerprint: &[u8; 32],
    budget: &mut Budget,
) -> Result<Option<CommitReceipt>> {
    let Some(reference) = snapshot
        .outcome_ref(transaction.request_id(), budget)
        .await?
    else {
        return Ok(None);
    };
    if &reference.fingerprint != fingerprint {
        return Err(Error::RequestIdReuse(transaction.request_id().clone()));
    }
    Ok(Some(
        snapshot
            .outcome(transaction.request_id(), &reference, budget)
            .await?
            .receipt,
    ))
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
        let revision = pinned.revision(&path("a")).await.unwrap().unwrap();
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
        barrier: tokio::sync::Barrier,
        synchronized_reads: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl obelisk_storage::Storage for ResponseFault {
        async fn read(
            &self,
            id: ObjectId,
            range: Option<std::ops::Range<u64>>,
        ) -> obelisk_storage::Result<bytes::Bytes> {
            let bytes = self.inner.read(id, range).await?;
            if self.mode.load(std::sync::atomic::Ordering::SeqCst) == 8
                && bytes.get(4) == Some(&5)
                && bytes.get(43) == Some(&2)
            {
                self.mode.store(0, std::sync::atomic::Ordering::SeqCst);
                return Err(obelisk_storage::Error::Transport(
                    "lost lazy request page read".into(),
                ));
            }
            Ok(bytes)
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
            let result = self.inner.read_head().await?;
            if self.mode.load(std::sync::atomic::Ordering::SeqCst) == 6
                && self
                    .synchronized_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    < 2
            {
                self.barrier.wait().await;
            }
            Ok(result)
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
                7 => {
                    self.mode.store(8, std::sync::atomic::Ordering::SeqCst);
                    Err(obelisk_storage::Error::Uncertain(
                        "lost success and lazy outcome lookup".into(),
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
        for mode in [1, 2, 3, 4, 7] {
            let dir = tempfile::tempdir().unwrap();
            let fault = Arc::new(ResponseFault {
                inner: Arc::new(
                    obelisk_storage::folder::Folder::open(dir.path())
                        .await
                        .unwrap(),
                ),
                mode: std::sync::atomic::AtomicU8::new(0),
                barrier: tokio::sync::Barrier::new(2),
                synchronized_reads: std::sync::atomic::AtomicUsize::new(0),
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
            if mode == 3 || mode == 4 || mode == 7 {
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
    #[tokio::test]
    async fn head_conflict_recomputes_patches_from_the_latest_document() {
        let dir = tempfile::tempdir().unwrap();
        let fault = Arc::new(ResponseFault {
            inner: Arc::new(
                obelisk_storage::folder::Folder::open(dir.path())
                    .await
                    .unwrap(),
            ),
            mode: std::sync::atomic::AtomicU8::new(0),
            barrier: tokio::sync::Barrier::new(2),
            synchronized_reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let db = Engine::open(fault.clone(), EngineOptions::default())
            .await
            .unwrap();
        db.commit(&tx("seed", vec![put("a", 1)])).await.unwrap();
        fault.mode.store(6, std::sync::atomic::Ordering::SeqCst);
        let mut tasks = Vec::new();
        for name in ["left", "right"] {
            let db = db.clone();
            let t = tx(
                name,
                vec![Mutation {
                    path: path("a"),
                    condition: Condition::Any,
                    kind: MutationKind::Patch(
                        Patch::new([(field(name), Value::Boolean(true))], vec![]).unwrap(),
                    ),
                }],
            );
            tasks.push(tokio::spawn(async move { db.commit(&t).await.unwrap() }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let result = db.get(&path("a")).await.unwrap().unwrap();
        assert_eq!(
            result.document.get(&field("value")),
            Some(&Value::Integer(1))
        );
        assert_eq!(
            result.document.get(&field("left")),
            Some(&Value::Boolean(true))
        );
        assert_eq!(
            result.document.get(&field("right")),
            Some(&Value::Boolean(true))
        );
        assert_eq!(db.snapshot().await.unwrap().sequence(), 3);
    }
    #[tokio::test]
    async fn deleted_and_recreated_documents_never_reuse_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let db = engine(dir.path()).await;
        let first = db.commit(&tx("original", vec![put("a", 1)])).await.unwrap();
        let revision = first.revisions[&path("a")].clone().unwrap();
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
        let recreated = db.commit(&tx("recreate", vec![put("a", 1)])).await.unwrap();
        assert_ne!(recreated.revisions[&path("a")].as_ref(), Some(&revision));
        assert!(matches!(
            db.commit(&tx(
                "stale-revision",
                vec![Mutation {
                    condition: Condition::Revision(revision),
                    ..put("a", 2)
                }]
            ))
            .await,
            Err(Error::RevisionConflict(_))
        ));
        assert_eq!(
            db.commit(&tx("original", vec![put("a", 1)])).await.unwrap(),
            first
        );
        assert_eq!(db.snapshot().await.unwrap().sequence(), 3);
    }
}
