#![cfg(unix)]
use crate::{
    format,
    metadata::{self, Family, KeyRange},
    *,
};
use obelisk_storage::{ObjectId, Storage};
use obelisk_types::*;
use std::{collections::BTreeMap, ops::Bound, sync::Arc};
fn path(s: &str) -> DocumentPath {
    DocumentPath::parse(s).unwrap()
}
fn transaction(id: &str, paths: Vec<DocumentPath>) -> Transaction {
    Transaction::new(
        RequestId::new(id).unwrap(),
        paths
            .into_iter()
            .map(|path| Mutation {
                path,
                condition: Condition::Any,
                kind: MutationKind::Put(Document::default()),
            })
            .collect(),
        vec![],
    )
    .unwrap()
}
async fn db(store: Arc<dyn Storage>) -> Engine {
    Engine::open(store, EngineOptions::default()).await.unwrap()
}

// Rebuild only these bounded test fixtures, preserving every logical entry.
// Production updates still edit the relevant paths of the existing trees.
async fn equivalent_rewrite(
    snapshot: &Snapshot,
) -> (
    metadata::Root,
    Vec<(ObjectId, bytes::Bytes)>,
    metadata::Budget,
) {
    let mut budget = snapshot.resources.budget();
    let mut root = snapshot.manifest.clone();
    root.sequence += 1;
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let mut objects = Vec::new();
    for (family, old, new) in [
        (
            Family::Documents,
            snapshot.manifest.documents.as_ref(),
            &mut root.documents,
        ),
        (
            Family::Requests,
            snapshot.manifest.requests.as_ref(),
            &mut root.requests,
        ),
        (
            Family::Membership,
            snapshot.manifest.membership.as_ref(),
            &mut root.membership,
        ),
    ] {
        let rows = metadata::range(
            snapshot.store.as_ref(),
            family,
            old,
            KeyRange {
                start: Bound::Unbounded,
                end: None,
            },
            metadata::MAX_ENTRIES,
            &mut budget,
        )
        .await
        .unwrap();
        assert!(
            rows.len() < metadata::MAX_ENTRIES,
            "fixture must fit one scan"
        );
        let changes = rows.into_iter().map(|(k, v)| (k, Some(v))).collect();
        let tree = metadata::prepare(snapshot.store.as_ref(), family, None, &changes, &mut budget)
            .await
            .unwrap();
        assert!(!tree.objects.is_empty());
        assert_ne!(old, tree.root.as_ref());
        *new = tree.root;
        objects.extend(tree.objects);
    }
    (root, objects, budget)
}

async fn all_entries(snapshot: &Snapshot, children: bool) -> Vec<QueryEntry> {
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = if children {
            snapshot
                .children(
                    &QueryScope::path("parent").unwrap(),
                    PageOptions::default(),
                    cursor.as_ref(),
                )
                .await
        } else {
            snapshot
                .descendants(&QueryScope::Root, PageOptions::default(), cursor.as_ref())
                .await
        }
        .unwrap();
        entries.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return entries;
        }
    }
}

#[derive(Debug)]
struct CorruptDocuments(Arc<dyn Storage>);
#[async_trait::async_trait]
impl Storage for CorruptDocuments {
    async fn read(
        &self,
        id: ObjectId,
        range: Option<std::ops::Range<u64>>,
    ) -> obelisk_storage::Result<bytes::Bytes> {
        let bytes = self.0.read(id, range).await?;
        if bytes.get(4) == Some(&4) {
            let mut corrupt = bytes.to_vec();
            let last = corrupt.len() - 1;
            corrupt[last] ^= 1;
            Ok(corrupt.into())
        } else {
            Ok(bytes)
        }
    }
    async fn create(&self, id: ObjectId, bytes: bytes::Bytes) -> obelisk_storage::Result<()> {
        self.0.create(id, bytes).await
    }
    async fn read_head(&self) -> obelisk_storage::Result<Option<obelisk_storage::Head>> {
        self.0.read_head().await
    }
    async fn compare_exchange_head(
        &self,
        v: Option<&obelisk_storage::VersionToken>,
        b: bytes::Bytes,
    ) -> obelisk_storage::Result<obelisk_storage::VersionToken> {
        self.0.compare_exchange_head(v, b).await
    }
    async fn list(&self) -> obelisk_storage::Result<Vec<ObjectId>> {
        self.0.list().await
    }
    async fn delete(&self, id: ObjectId) -> obelisk_storage::Result<()> {
        self.0.delete(id).await
    }
}

#[derive(Debug)]
struct LostPublicationResponse(Arc<dyn Storage>);
#[async_trait::async_trait]
impl Storage for LostPublicationResponse {
    async fn read(
        &self,
        id: ObjectId,
        range: Option<std::ops::Range<u64>>,
    ) -> obelisk_storage::Result<bytes::Bytes> {
        self.0.read(id, range).await
    }
    async fn create(&self, id: ObjectId, bytes: bytes::Bytes) -> obelisk_storage::Result<()> {
        self.0.create(id, bytes).await
    }
    async fn read_head(&self) -> obelisk_storage::Result<Option<obelisk_storage::Head>> {
        self.0.read_head().await
    }
    async fn compare_exchange_head(
        &self,
        v: Option<&obelisk_storage::VersionToken>,
        b: bytes::Bytes,
    ) -> obelisk_storage::Result<obelisk_storage::VersionToken> {
        self.0.compare_exchange_head(v, b).await?;
        Err(obelisk_storage::Error::Uncertain(
            "lost metadata-only publication response".into(),
        ))
    }
    async fn list(&self) -> obelisk_storage::Result<Vec<ObjectId>> {
        self.0.list().await
    }
    async fn delete(&self, id: ObjectId) -> obelisk_storage::Result<()> {
        self.0.delete(id).await
    }
}
#[tokio::test]
async fn lazy_startup_reads_only_root_and_defers_document_corruption() {
    use obelisk_storage::metrics::InstrumentedStorage;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    engine
        .commit(&transaction(
            "seed",
            (0..256)
                .map(|i| path(&format!("docs/{i:04}{}", "x".repeat(300))))
                .collect(),
        ))
        .await
        .unwrap();
    let corrupt = Arc::new(CorruptDocuments(store));
    let measured = Arc::new(InstrumentedStorage::new(corrupt));
    let opened = db(measured.clone()).await;
    let stats = measured.stats();
    assert_eq!(stats.object_reads, 1);
    assert_eq!(stats.range_reads, 1);
    assert_eq!(stats.object_creates, 0);
    assert_eq!(stats.list_calls, 0);
    assert!(
        stats.bytes_read < (metadata::MAX_ROOT_BYTES + 2 * obelisk_storage::MAX_HEAD_BYTES) as u64
    );
    let first = path(&format!("docs/0000{}", "x".repeat(300)));
    assert!(matches!(opened.get(&first).await, Err(Error::Corrupt(_))));
}
#[tokio::test]
async fn metadata_only_publication_preserves_revisions_receipts_and_historical_roots() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    let t = transaction(
        "batch",
        (0..256)
            .map(|i| path(&format!("parent/{i:04}{}", "x".repeat(300))))
            .collect(),
    );
    let receipt = engine.commit(&t).await.unwrap();
    let old = engine.snapshot().await.unwrap();
    assert!(old.manifest.documents.as_ref().unwrap().level > 0);
    let key = &t.mutations()[0].path;
    let revision = old.revision(key).await.unwrap();
    let expected = all_entries(&old, false).await;
    let expected_children = all_entries(&old, true).await;
    let (root, objects, mut budget) = equivalent_rewrite(&old).await;
    let mut attempted = false;
    engine
        .publish_root(&old, &root, objects, &mut budget, &mut attempted)
        .await
        .unwrap();
    assert!(attempted);
    let current = engine.snapshot().await.unwrap();
    assert_eq!(current.sequence(), 2);
    assert_ne!(current.manifest.documents, old.manifest.documents);
    assert_ne!(current.manifest.requests, old.manifest.requests);
    assert_ne!(current.manifest.membership, old.manifest.membership);
    assert_ne!(current.commit_id(), old.commit_id());
    assert_eq!(current.revision(key).await.unwrap(), revision);
    assert_eq!(all_entries(&current, false).await, expected);
    assert_eq!(all_entries(&current, true).await, expected_children);
    assert_eq!(
        engine.resolve(t.request_id()).await.unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(engine.commit(&t).await.unwrap(), receipt);
    assert_eq!(
        engine
            .snapshot_at(old.reference())
            .await
            .unwrap()
            .sequence(),
        1
    );
    let next = transaction("next", vec![path("new")]);
    let next_receipt = engine.commit(&next).await.unwrap();
    let latest = engine.snapshot().await.unwrap();
    let expected_latest = all_entries(&latest, false).await;
    let (stale_root, stale_objects, mut stale_budget) = equivalent_rewrite(&current).await;
    attempted = false;
    assert!(matches!(
        engine
            .publish_root(
                &current,
                &stale_root,
                stale_objects,
                &mut stale_budget,
                &mut attempted
            )
            .await,
        Err(Error::Storage(obelisk_storage::Error::Conflict))
    ));
    assert!(attempted);
    assert_eq!(
        engine.snapshot().await.unwrap().reference(),
        latest.reference()
    );
    assert!(engine.get(&path("new")).await.unwrap().is_some());

    // Retry the operation from authoritative state, including the intervening
    // transaction's document, membership and outcome; never attach stale roots.
    let fresh = engine.snapshot().await.unwrap();
    let (retry_root, retry_objects, mut retry_budget) = equivalent_rewrite(&fresh).await;
    attempted = false;
    engine
        .publish_root(
            &fresh,
            &retry_root,
            retry_objects,
            &mut retry_budget,
            &mut attempted,
        )
        .await
        .unwrap();
    let reopened = db(store).await;
    let rewritten = reopened.snapshot().await.unwrap();
    assert_eq!(rewritten.sequence(), latest.sequence() + 1);
    assert_eq!(all_entries(&rewritten, false).await, expected_latest);
    assert_eq!(all_entries(&rewritten, true).await, expected_children);
    assert_eq!(reopened.commit(&t).await.unwrap(), receipt);
    assert_eq!(reopened.commit(&next).await.unwrap(), next_receipt);
    let historical = reopened.snapshot_at(old.reference()).await.unwrap();
    assert_eq!(all_entries(&historical, false).await, expected);
    assert!(historical.get(&path("new")).await.unwrap().is_none());
    let bytes = metadata::encode_head(&current.reference).unwrap();
    assert!(matches!(
        format::decode_head(&bytes),
        Err(Error::UnsupportedVersion(2))
    ));
}

#[tokio::test]
async fn metadata_only_rewrite_lost_response_remains_explicit_and_reopens() {
    use crate::diagnostics::{InstrumentedMetadata, ObjectKind};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    let t = transaction("seed", vec![path("parent/a"), path("parent/b")]);
    let original = engine.commit(&t).await.unwrap();
    let old = engine.snapshot().await.unwrap();
    let expected = all_entries(&old, false).await;
    let (root, objects, mut budget) = equivalent_rewrite(&old).await;
    let measured = Arc::new(InstrumentedMetadata::new(store.clone()));
    let faulty = db(Arc::new(LostPublicationResponse(measured.clone()))).await;
    let mut attempted = false;
    assert!(matches!(
        faulty
            .publish_root(&old, &root, objects, &mut budget, &mut attempted)
            .await,
        Err(Error::Storage(obelisk_storage::Error::Uncertain(_)))
    ));
    assert!(attempted, "the injected response loss must reach HEAD CAS");
    for kind in [ObjectKind::Segment, ObjectKind::Record, ObjectKind::Outcome] {
        assert_eq!(measured.stats(kind).creates, 0);
    }
    for kind in [
        ObjectKind::Root,
        ObjectKind::Documents,
        ObjectKind::Membership,
        ObjectKind::Requests,
    ] {
        assert!(measured.stats(kind).creates > 0);
    }
    let reopened = db(store).await;
    let observed = reopened.snapshot().await.unwrap();
    assert_eq!(observed.commit_id(), &root.commit_id);
    assert_eq!(observed.sequence(), old.sequence() + 1);
    assert_eq!(all_entries(&observed, false).await, expected);
    assert_eq!(all_entries(&observed, true).await, expected);
    assert_eq!(reopened.commit(&t).await.unwrap(), original);
    assert_eq!(
        reopened.resolve(t.request_id()).await.unwrap(),
        Some(original)
    );
    assert_eq!(
        all_entries(&reopened.snapshot_at(old.reference()).await.unwrap(), false).await,
        expected
    );
}
#[tokio::test]
async fn hierarchy_membership_tracks_existence_without_rewriting_on_patch() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    engine
        .commit(&transaction(
            "seed",
            vec![
                path("a"),
                path("a/b"),
                path("a/b/c"),
                DocumentPath::from_segments(["a/b", "literal"]).unwrap(),
            ],
        ))
        .await
        .unwrap();
    let before = engine.snapshot().await.unwrap();
    let patch = Transaction::new(
        RequestId::new("patch").unwrap(),
        vec![Mutation {
            path: path("a/b"),
            condition: Condition::Any,
            kind: MutationKind::Patch(
                Patch::new(
                    [(FieldPath::parse("value").unwrap(), Value::Integer(1))],
                    vec![],
                )
                .unwrap(),
            ),
        }],
        vec![],
    )
    .unwrap();
    engine.commit(&patch).await.unwrap();
    assert_eq!(
        before.manifest.membership,
        engine.snapshot().await.unwrap().manifest.membership
    );
    let delete = Transaction::new(
        RequestId::new("delete").unwrap(),
        vec![Mutation {
            path: path("a/b"),
            condition: Condition::Any,
            kind: MutationKind::Delete,
        }],
        vec![],
    )
    .unwrap();
    engine.commit(&delete).await.unwrap();
    let snapshot = engine.snapshot().await.unwrap();
    for p in [
        path("a"),
        path("a/b/c"),
        DocumentPath::from_segments(["a/b", "literal"]).unwrap(),
    ] {
        assert_eq!(
            metadata::get(
                store.as_ref(),
                Family::Membership,
                snapshot.manifest.membership.as_ref(),
                &metadata::membership_key(&p),
                &mut metadata::Budget::default()
            )
            .await
            .unwrap(),
            Some(vec![])
        );
    }
    assert!(
        metadata::get(
            store.as_ref(),
            Family::Membership,
            snapshot.manifest.membership.as_ref(),
            &metadata::membership_key(&path("a/b")),
            &mut metadata::Budget::default()
        )
        .await
        .unwrap()
        .is_none()
    );
    let all = metadata::range(
        store.as_ref(),
        Family::Membership,
        snapshot.manifest.membership.as_ref(),
        KeyRange {
            start: Bound::Unbounded,
            end: None,
        },
        256,
        &mut metadata::Budget::default(),
    )
    .await
    .unwrap();
    assert_eq!(all.len(), 3);
    assert!(before.get(&path("a/b")).await.unwrap().is_some());
    assert!(snapshot.get(&path("a/b")).await.unwrap().is_none());
}
#[tokio::test]
async fn v1_databases_are_rejected_without_writes_and_limits_fail_before_publication() {
    use obelisk_storage::metrics::InstrumentedStorage;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let root = format::Manifest {
        database_id: DatabaseId::new("old-db").unwrap(),
        commit_id: CommitId::new("old-commit").unwrap(),
        sequence: 0,
        latest_segment: None,
        documents: BTreeMap::new(),
        requests: BTreeMap::new(),
    };
    let bytes = format::encode_manifest(&root).unwrap();
    let id = ObjectId::random();
    let head = format::HeadRef {
        database_id: root.database_id,
        commit_id: root.commit_id,
        sequence: 0,
        manifest: id,
        manifest_hash: *blake3::hash(&bytes).as_bytes(),
    };
    store.create(id, bytes.into()).await.unwrap();
    store
        .compare_exchange_head(None, format::encode_head(&head).unwrap().into())
        .await
        .unwrap();
    let measured = Arc::new(InstrumentedStorage::new(store));
    assert!(matches!(
        Engine::open(measured.clone(), EngineOptions::default()).await,
        Err(Error::UnsupportedVersion(1))
    ));
    assert_eq!(measured.stats().object_creates, 0);
    assert_eq!(measured.stats().head_cas_attempts, 0);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    let original = engine.snapshot().await.unwrap().reference();
    let options = EngineOptions {
        resources: ResourceLimits {
            prepared_metadata_bytes: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let limited = Engine::open(store, options).await.unwrap();
    assert!(matches!(
        limited
            .commit(&transaction("too-small", vec![path("a")]))
            .await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), original);
    assert!(
        engine
            .resolve(&RequestId::new("too-small").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn maximum_batch_paths_remain_readable_across_split_and_collapse() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = db(store.clone()).await;
    let paths: Vec<_> = (0..256)
        .map(|i| path(&format!("{i:04}{}", "z".repeat(MAX_PATH_BYTES - 4))))
        .collect();
    let request = transaction("max-path-batch", paths.clone());
    let receipt = engine.commit(&request).await.unwrap();
    let old = engine.snapshot().await.unwrap();
    assert!(old.manifest.documents.as_ref().unwrap().level >= 2);
    for p in [&paths[0], &paths[255]] {
        assert!(old.get(p).await.unwrap().is_some());
    }
    let delete = Transaction::new(
        RequestId::new("delete-most").unwrap(),
        paths[..255]
            .iter()
            .map(|path| Mutation {
                path: path.clone(),
                condition: Condition::Any,
                kind: MutationKind::Delete,
            })
            .collect(),
        vec![],
    )
    .unwrap();
    engine.commit(&delete).await.unwrap();
    let current = engine.snapshot().await.unwrap();
    assert_eq!(current.manifest.documents.as_ref().unwrap().level, 0);
    assert!(current.get(&paths[0]).await.unwrap().is_none());
    assert!(old.get(&paths[0]).await.unwrap().is_some());
    let reopened = db(store).await;
    assert_eq!(
        reopened.resolve(request.request_id()).await.unwrap(),
        Some(receipt)
    );
    assert!(reopened.get(&paths[255]).await.unwrap().is_some());
}
