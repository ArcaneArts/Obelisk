#![cfg(all(feature = "folder", unix))]
use obelisk::*;
use std::sync::Arc;
fn path(p: &str) -> DocumentPath {
    DocumentPath::parse(p).unwrap()
}
fn doc(n: i64) -> Document {
    Document::new([(FieldPath::parse("n").unwrap(), Value::Integer(n))]).unwrap()
}
async fn seed(db: &Database, paths: &[DocumentPath]) {
    let mutations = paths
        .iter()
        .enumerate()
        .map(|(i, path)| Mutation {
            path: path.clone(),
            condition: Condition::Any,
            kind: MutationKind::Put(doc(i as i64)),
        })
        .collect();
    db.commit(&Transaction::new(RequestId::new("seed").unwrap(), mutations, vec![]).unwrap())
        .await
        .unwrap();
}
async fn collect(db: &Database, scope: &QueryScope, children: bool) -> Vec<QueryEntry> {
    let mut cursor = None;
    let mut out = Vec::new();
    let options = PageOptions::new(2).unwrap();
    let mut reference = None;
    loop {
        let page = if children {
            db.children(scope, options, cursor.as_ref()).await
        } else {
            db.descendants(scope, options, cursor.as_ref()).await
        }
        .unwrap();
        if let Some(old) = &reference {
            assert_eq!(old, &page.snapshot);
        } else {
            reference = Some(page.snapshot.clone());
        }
        out.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return out;
        }
    }
}
fn paths(entries: &[QueryEntry]) -> Vec<DocumentPath> {
    entries.iter().map(|e| e.path.clone()).collect()
}

#[tokio::test]
async fn hierarchy_matches_structured_reference_with_missing_parents_and_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    let mut expected = vec![
        path("a"),
        path("a/b"),
        path("a/b/c"),
        path("a/bc"),
        path("a/bc/d"),
        path("orphan/deep/doc"),
        path("../leaf"),
        path("é/🙂"),
        DocumentPath::from_segments(["a/b"]).unwrap(),
        DocumentPath::from_segments(["a/b", "literal"]).unwrap(),
    ];
    seed(&db, &expected).await;
    expected.sort();
    for scope in [
        QueryScope::Root,
        QueryScope::path("a").unwrap(),
        QueryScope::path("a/b").unwrap(),
        QueryScope::path("orphan").unwrap(),
        QueryScope::path("orphan/deep").unwrap(),
        QueryScope::Path(DocumentPath::from_segments(["a/b"]).unwrap()),
        QueryScope::path("missing").unwrap(),
    ] {
        for children in [true, false] {
            let reference: Vec<_> = expected
                .iter()
                .filter(|p| match &scope {
                    QueryScope::Root => !children || p.segments().len() == 1,
                    QueryScope::Path(parent) => {
                        parent.is_ancestor_of(p)
                            && (!children || p.segments().len() == parent.segments().len() + 1)
                    }
                })
                .cloned()
                .collect();
            assert_eq!(paths(&collect(&db, &scope, children).await), reference);
        }
    }
    db.delete("delete-parent", "a/b", Condition::Any)
        .await
        .unwrap();
    assert_eq!(
        paths(&collect(&db, &QueryScope::path("a/b").unwrap(), true).await),
        vec![path("a/b/c")]
    );
    assert_eq!(
        paths(&collect(&db, &QueryScope::path("a").unwrap(), false).await),
        vec![path("a/b/c"), path("a/bc"), path("a/bc/d")]
    );
}
#[tokio::test]
async fn bulk_preserves_order_duplicates_missing_entries_and_pinned_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    seed(&db, &[path("a"), path("a/b")]).await;
    let pinned = db.snapshot().await.unwrap();
    let result = db.get_many(&["a/b", "missing", "a/b", "a"]).await.unwrap();
    assert!(result[1].is_none());
    assert_eq!(result[0], result[2]);
    assert_eq!(result[0].as_ref().unwrap().document, doc(1));
    assert_eq!(result[3].as_ref().unwrap().document, doc(0));
    db.put("update", "a/b", doc(9), Condition::Any)
        .await
        .unwrap();
    assert_eq!(pinned.get_many(&[path("a/b")]).await.unwrap()[0], result[0]);
    assert_eq!(
        db.get_many(&["a/b"]).await.unwrap()[0]
            .as_ref()
            .unwrap()
            .document,
        doc(9)
    );
    assert!(db.get_many(&[]).await.unwrap().is_empty());
    assert!(matches!(
        db.get_many(&vec!["a"; 257]).await,
        Err(Error::ResourceLimit(_))
    ));
}
#[tokio::test]
async fn bad_bulk_input_is_rejected_before_storage_reads() {
    let dir = tempfile::tempdir().unwrap();
    let measured = Arc::new(storage::metrics::InstrumentedStorage::new(Arc::new(
        storage::folder::Folder::open(dir.path()).await.unwrap(),
    )));
    let db = Database::open_with_storage(measured.clone(), DatabaseOptions::default())
        .await
        .unwrap();
    let before = measured.stats();
    assert!(matches!(
        db.get_many(&["valid", "invalid//path"]).await,
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        db.get_many(&vec!["a"; 257]).await,
        Err(Error::ResourceLimit(_))
    ));
    assert!(db.get_many(&[]).await.unwrap().is_empty());
    let after = measured.stats();
    assert_eq!(before.head_reads, after.head_reads);
    assert_eq!(before.object_reads, after.object_reads);
}
#[tokio::test]
async fn continuations_pin_data_across_writes_and_reopened_engines() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    let expected: Vec<_> = (0..19).map(|i| path(&format!("docs/{i:03}"))).collect();
    seed(&db, &expected).await;
    let scope = QueryScope::path("docs").unwrap();
    let options = PageOptions::new(3).unwrap();
    let first = db.children(&scope, options, None).await.unwrap();
    let old = first.snapshot.clone();
    let mut entries = first.entries;
    let mut cursor = first.next;
    db.delete("delete", "docs/004", Condition::Any)
        .await
        .unwrap();
    db.put("replace", "docs/005", doc(99), Condition::Any)
        .await
        .unwrap();
    db.create("insert", "docs/003a", doc(88)).await.unwrap();
    let reopened = Database::open_folder(dir.path()).await.unwrap();
    while let Some(token) = cursor {
        let page = reopened
            .children(&scope, PageOptions::new(4).unwrap(), Some(&token))
            .await
            .unwrap();
        assert_eq!(page.snapshot, old);
        entries.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(paths(&entries), expected);
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.document, doc(i as i64));
    }
    assert_eq!(collect(&db, &scope, true).await.len(), 19);
}
#[tokio::test]
async fn cursor_scope_kind_snapshot_and_database_mismatches_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    seed(&db, &[path("a/1"), path("a/2"), path("a/3")]).await;
    let scope = QueryScope::path("a").unwrap();
    let options = PageOptions::new(1).unwrap();
    let first = db.children(&scope, options, None).await.unwrap();
    let cursor = first.next.unwrap();
    assert!(matches!(
        db.children(&QueryScope::Root, options, Some(&cursor)).await,
        Err(Error::InvalidCursor(_))
    ));
    assert!(matches!(
        db.descendants(&scope, options, Some(&cursor)).await,
        Err(Error::InvalidCursor(_))
    ));
    db.put("change", "a/3", doc(10), Condition::Any)
        .await
        .unwrap();
    let latest = db.snapshot().await.unwrap();
    assert!(matches!(
        latest.children(&scope, options, Some(&cursor)).await,
        Err(Error::InvalidCursor(_))
    ));
    let other = tempfile::tempdir().unwrap();
    let other = Database::open_folder(other.path()).await.unwrap();
    assert!(matches!(
        other.children(&scope, options, Some(&cursor)).await,
        Err(Error::InvalidCursor(_))
    ));
}
#[tokio::test]
async fn byte_bounded_pages_resume_without_skipping_prefetched_documents() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    let expected: Vec<_> = (0..7).map(|i| path(&format!("docs/{i}"))).collect();
    seed(&db, &expected).await;
    let options = PageOptions::new(100).unwrap().with_byte_limit(250).unwrap();
    let scope = QueryScope::path("docs").unwrap();
    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let page = db
            .descendants(&scope, options, cursor.as_ref())
            .await
            .unwrap();
        assert!(!page.entries.is_empty());
        assert!(page.entries.len() <= 2);
        found.extend(paths(&page.entries));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(found, expected);
    assert!(matches!(
        db.children(
            &scope,
            PageOptions::default().with_byte_limit(1).unwrap(),
            None
        )
        .await,
        Err(Error::ResourceLimit(_))
    ));
}
