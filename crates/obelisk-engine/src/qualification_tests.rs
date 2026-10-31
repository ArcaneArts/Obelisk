#![cfg(unix)]
use crate::{
    diagnostics::{InstrumentedMetadata, ObjectKind},
    metadata::{self, Family, KeyRange},
    *,
};
use obelisk_storage::Storage;
use obelisk_types::*;
use std::{ops::Bound, sync::Arc};
fn path(p: &str) -> DocumentPath {
    DocumentPath::parse(p).unwrap()
}
fn put(id: &str, paths: Vec<DocumentPath>, document: Document) -> Transaction {
    Transaction::new(
        RequestId::new(id).unwrap(),
        paths
            .into_iter()
            .map(|path| Mutation {
                path,
                condition: Condition::Any,
                kind: MutationKind::Put(document.clone()),
            })
            .collect(),
        vec![],
    )
    .unwrap()
}
async fn store(dir: &std::path::Path) -> Arc<dyn Storage> {
    Arc::new(obelisk_storage::folder::Folder::open(dir).await.unwrap())
}
async fn db(store: Arc<dyn Storage>) -> Engine {
    Engine::open(store, EngineOptions::default()).await.unwrap()
}

#[tokio::test]
async fn counters_prove_root_only_opening_local_updates_and_selective_children() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path()).await;
    let engine = db(store.clone()).await;
    engine
        .commit(&put("parent", vec![path("a/b")], Document::default()))
        .await
        .unwrap();
    for batch in 0..12 {
        engine
            .commit(&put(
                &format!("seed-{batch}"),
                (batch * 256..(batch + 1) * 256)
                    .map(|i| path(&format!("a/b/deep/{i:05}")))
                    .collect(),
                Document::default(),
            ))
            .await
            .unwrap();
    }
    let diagnostics = Arc::new(InstrumentedMetadata::new(store));
    let engine = db(diagnostics.clone()).await;
    assert_eq!(diagnostics.stats(ObjectKind::Root).reads, 1);
    assert_eq!(
        diagnostics.stats(ObjectKind::Root).reader_decode_charge,
        1284
    );
    for kind in [
        ObjectKind::Documents,
        ObjectKind::Requests,
        ObjectKind::Membership,
        ObjectKind::Outcome,
        ObjectKind::Record,
    ] {
        assert_eq!(diagnostics.stats(kind).reads, 0);
    }
    let snapshot = engine.snapshot().await.unwrap();
    let member_height = snapshot.manifest.membership.as_ref().unwrap().level;
    let page = engine
        .children(
            &QueryScope::path("a").unwrap(),
            PageOptions::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].path, path("a/b"));
    assert!(page.next.is_none());
    assert!(diagnostics.stats(ObjectKind::Membership).reads <= u64::from(member_height) + 2);
    assert_eq!(diagnostics.stats(ObjectKind::Record).reads, 1);
    assert_eq!(diagnostics.stats(ObjectKind::Requests).reads, 0);
    let docs_before = diagnostics.stats(ObjectKind::Documents).creates;
    let members_before = diagnostics.stats(ObjectKind::Membership).creates;
    engine
        .commit(&put(
            "update-parent",
            vec![path("a/b")],
            Document::default(),
        ))
        .await
        .unwrap();
    assert!(
        diagnostics.stats(ObjectKind::Documents).creates - docs_before
            <= 2 * (u64::from(snapshot.manifest.documents.as_ref().unwrap().level) + 1)
    );
    assert_eq!(
        diagnostics.stats(ObjectKind::Membership).creates,
        members_before
    );
}

#[tokio::test]
async fn bulk_and_query_budgets_bound_work_without_partial_vectors_or_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path()).await;
    let engine = db(store.clone()).await;
    let document = Document::new([(
        FieldPath::parse("payload").unwrap(),
        Value::Binary(vec![0; MAX_DOCUMENT_BYTES - 1024]),
    )])
    .unwrap();
    engine
        .commit(&put("large", vec![path("large")], document))
        .await
        .unwrap();
    let diagnostics = Arc::new(InstrumentedMetadata::new(store.clone()));
    let engine = db(diagnostics.clone()).await;
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        engine.get_many(&vec![path("large"); 17]).await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(diagnostics.stats(ObjectKind::Record).reads, 1);
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    engine
        .commit(&put(
            "small",
            vec![path("small")],
            Document::new([(
                FieldPath::parse("payload").unwrap(),
                Value::Binary(vec![0; 4096]),
            )])
            .unwrap(),
        ))
        .await
        .unwrap();
    let limited = Engine::open(
        store.clone(),
        EngineOptions {
            resources: ResourceLimits {
                decoded_bytes: 32 * 1024,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        limited.get_many(&vec![path("small"); 256]).await,
        Err(Error::ResourceLimit(_))
    ));
    let limited = Engine::open(
        store.clone(),
        EngineOptions {
            resources: ResourceLimits {
                metadata_page_reads: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        limited
            .children(&QueryScope::Root, PageOptions::default(), None)
            .await,
        Err(Error::ResourceLimit(_))
    ));
    assert!(
        Engine::open(
            store,
            EngineOptions {
                resources: ResourceLimits {
                    metadata_page_reads: 0,
                    ..Default::default()
                },
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn large_document_pages_stop_at_bytes_and_resume_the_same_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let engine = db(store(dir.path()).await).await;
    let document = Document::new([(
        FieldPath::parse("payload").unwrap(),
        Value::Binary(vec![0; MAX_DOCUMENT_BYTES - 1024]),
    )])
    .unwrap();
    for batch in 0..2 {
        engine
            .commit(&put(
                &format!("large-{batch}"),
                (batch * 10..batch * 10 + 10)
                    .map(|i| path(&format!("docs/{i:03}")))
                    .collect(),
                document.clone(),
            ))
            .await
            .unwrap();
    }
    let scope = QueryScope::path("docs").unwrap();
    let first = engine
        .descendants(&scope, PageOptions::default(), None)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 16);
    let token = first.next.unwrap();
    let second = engine
        .descendants(&scope, PageOptions::default(), Some(&token))
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 4);
    assert!(second.next.is_none());
    assert_eq!(second.snapshot, first.snapshot);
    assert_eq!(first.entries.last().unwrap().path, path("docs/015"));
    assert_eq!(second.entries.first().unwrap().path, path("docs/016"));
}

/// Deliberate capacity qualification, not part of routine CI or actual-S3 tests.
#[tokio::test]
#[ignore = "large disposable POSIX capacity workload; run release mode explicitly"]
async fn paged_capacity_exceeds_the_old_manifest_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path()).await;
    let engine = db(store.clone()).await;
    let padding = "x".repeat(800);
    let mut first = None;
    let mut first_request = None;
    for start in (0_usize..100_000).step_by(256) {
        let end = (start + 256).min(100_000);
        let paths = (start..end)
            .map(|i| path(&format!("capacity/{i:06}{padding}")))
            .collect();
        let tx = put(&format!("capacity-{start}"), paths, Document::default());
        let receipt = engine.commit(&tx).await.unwrap();
        if first.is_none() {
            first = Some(receipt);
            first_request = Some(tx);
        }
        if end >= 100_000 || end.is_multiple_of(10_240) {
            eprintln!("capacity: {end} acknowledged documents");
        }
    }
    let pinned = engine.snapshot().await.unwrap();
    let mut last = None::<Vec<u8>>;
    let mut count = 0;
    let mut live_bytes = 0;
    loop {
        let rows = metadata::range(
            store.as_ref(),
            Family::Documents,
            pinned.manifest.documents.as_ref(),
            KeyRange {
                start: last
                    .as_deref()
                    .map(Bound::Excluded)
                    .unwrap_or(Bound::Unbounded),
                end: None,
            },
            4096,
            &mut metadata::Budget::default(),
        )
        .await
        .unwrap();
        if rows.is_empty() {
            break;
        }
        count += rows.len();
        live_bytes += rows.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        last = Some(rows.last().unwrap().0.clone());
    }
    assert_eq!(count, 100_000);
    assert!(live_bytes > 64 * 1024 * 1024);
    assert!(pinned.manifest.documents.as_ref().unwrap().level >= 2);
    let diagnostics = Arc::new(InstrumentedMetadata::new(store));
    let reopened = db(diagnostics.clone()).await;
    assert_eq!(diagnostics.stats(ObjectKind::Root).reads, 1);
    assert_eq!(diagnostics.stats(ObjectKind::Root).bytes_read, 386);
    assert_eq!(diagnostics.stats(ObjectKind::Documents).reads, 0);
    let first_path = path(&format!("capacity/000000{padding}"));
    assert!(reopened.get(&first_path).await.unwrap().is_some());
    assert!(
        reopened
            .get(&path(&format!("capacity/099999{padding}")))
            .await
            .unwrap()
            .is_some()
    );
    let before = diagnostics.stats(ObjectKind::Documents).creates;
    reopened
        .commit(&put(
            "capacity-update",
            vec![first_path.clone()],
            Document::new([(FieldPath::parse("n").unwrap(), Value::Integer(1))]).unwrap(),
        ))
        .await
        .unwrap();
    let pages = diagnostics.stats(ObjectKind::Documents).creates - before;
    assert!(pages <= 2 * (u64::from(pinned.manifest.documents.as_ref().unwrap().level) + 1));
    assert_eq!(diagnostics.stats(ObjectKind::Membership).creates, 0);
    assert!(
        pinned
            .get(&first_path)
            .await
            .unwrap()
            .unwrap()
            .document
            .fields()
            .is_empty()
    );
    assert_eq!(
        reopened.commit(&first_request.unwrap()).await.unwrap(),
        first.unwrap()
    );
    eprintln!(
        "capacity verified: {count} live documents; {live_bytes} live location key/value bytes; 386 root bytes; {pages} location pages for one update; original receipt and historical snapshot retained"
    );
}
