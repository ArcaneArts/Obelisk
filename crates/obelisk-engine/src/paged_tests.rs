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
    let mut root = old.manifest.clone();
    root.sequence += 1;
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let mut attempted = false;
    engine
        .publish_root(
            &old,
            &root,
            vec![],
            &mut metadata::Budget::default(),
            &mut attempted,
        )
        .await
        .unwrap();
    assert!(attempted);
    let current = engine.snapshot().await.unwrap();
    assert_eq!(current.sequence(), 2);
    assert_eq!(current.manifest.documents, old.manifest.documents);
    assert_eq!(current.revision(key).await.unwrap(), revision);
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
    engine
        .commit(&transaction("next", vec![path("new")]))
        .await
        .unwrap();
    assert!(matches!(
        engine
            .publish_root(
                &old,
                &root,
                vec![],
                &mut metadata::Budget::default(),
                &mut attempted
            )
            .await,
        Err(Error::Storage(obelisk_storage::Error::Conflict))
    ));
    assert!(engine.get(&path("new")).await.unwrap().is_some());
    let bytes = metadata::encode_head(&current.reference).unwrap();
    assert!(matches!(
        format::decode_head(&bytes),
        Err(Error::UnsupportedVersion(2))
    ));
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
