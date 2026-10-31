#![cfg(unix)]
use crate::{
    diagnostics::{InstrumentedMetadata, ObjectKind},
    index_tests::{definition, finish, folder, open, path, put, request, tx},
    metadata::{self, Family, Node},
    *,
};
use obelisk_types::*;
use std::sync::Arc;
fn query(definition: &IndexDefinition, value: i64, projection: Projection) -> IndexedQuery {
    IndexedQuery::new(
        definition.scope().clone(),
        vec![
            EqualityPredicate::new(FieldPath::parse("score").unwrap(), Value::Integer(value))
                .unwrap(),
        ],
        None,
        vec![],
        projection,
    )
    .unwrap()
    .with_index(definition.id().clone())
}
#[tokio::test]
async fn indexed_open_selective_work_and_local_edits_preserve_unrelated_subtrees() {
    let dir = tempfile::tempdir().unwrap();
    let raw = folder(&dir).await;
    let engine = open(raw.clone()).await;
    for start in (0..1024).step_by(128) {
        engine
            .commit(&tx(
                &format!("seed-{start}"),
                (start..start + 128)
                    .map(|n| put(&format!("people/{n:04}"), Value::Integer(0)))
                    .collect(),
            ))
            .await
            .unwrap();
    }
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    finish(&engine, &created.handle, "finish", 64).await;
    let diagnostics = Arc::new(InstrumentedMetadata::new(raw));
    let engine = open(diagnostics.clone()).await;
    assert_eq!(diagnostics.stats(ObjectKind::Root).reads, 1);
    for kind in [
        ObjectKind::Documents,
        ObjectKind::Requests,
        ObjectKind::Membership,
        ObjectKind::IndexDefinitions,
        ObjectKind::ScalarIndexes,
        ObjectKind::Outcome,
        ObjectKind::Record,
        ObjectKind::DefinitionObject,
        ObjectKind::IndexDescriptor,
        ObjectKind::Projection,
    ] {
        assert_eq!(diagnostics.stats(kind).reads, 0);
    }
    let before = engine.snapshot().await.unwrap();
    let mut budget = before.resources.budget();
    let index = before
        .load_index(definition.id(), &mut budget)
        .await
        .unwrap()
        .unwrap();
    let root = index.descriptor.active.unwrap().entries.unwrap();
    assert_eq!(root.level, 1);
    let old = Node::decode(
        &root
            .blob
            .read(diagnostics.as_ref(), metadata::MAX_PAGE_BYTES, &mut budget)
            .await
            .unwrap(),
        Family::ScalarIndexes,
    )
    .unwrap();
    assert!(old.entries.len() > 3);
    let pages = diagnostics.stats(ObjectKind::ScalarIndexes).creates;
    engine
        .commit(&tx("edit", vec![put("people/0000", Value::Integer(-1))]))
        .await
        .unwrap();
    assert_eq!(
        diagnostics.stats(ObjectKind::ScalarIndexes).creates - pages,
        2
    );
    let after = engine.snapshot().await.unwrap();
    let mut budget = after.resources.budget();
    let index = after
        .load_index(definition.id(), &mut budget)
        .await
        .unwrap()
        .unwrap();
    let root = index.descriptor.active.unwrap().entries.unwrap();
    let new = Node::decode(
        &root
            .blob
            .read(diagnostics.as_ref(), metadata::MAX_PAGE_BYTES, &mut budget)
            .await
            .unwrap(),
        Family::ScalarIndexes,
    )
    .unwrap();
    assert_eq!(
        new.entries[1..],
        old.entries[1..],
        "unrelated scalar subtrees were rewritten"
    );
    let records = diagnostics.stats(ObjectKind::Record).reads;
    let scalar = diagnostics.stats(ObjectKind::ScalarIndexes).reads;
    let documents = diagnostics.stats(ObjectKind::Documents).reads;
    let page = engine
        .query(
            &query(&definition, -1, Projection::Full),
            PageOptions::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].path, path("people/0000"));
    assert!(page.next.is_none());
    assert_eq!(diagnostics.stats(ObjectKind::Record).reads - records, 1);
    assert!(
        diagnostics.stats(ObjectKind::ScalarIndexes).reads - scalar <= u64::from(root.level) + 2
    );
    assert!(
        diagnostics.stats(ObjectKind::Documents).reads - documents
            <= u64::from(after.manifest.documents.as_ref().unwrap().level) + 2
    );
    let records = diagnostics.stats(ObjectKind::Record).reads;
    let projections = diagnostics.stats(ObjectKind::Projection).reads;
    let page = engine
        .query(
            &query(
                &definition,
                -1,
                Projection::fields(vec![
                    FieldPath::parse("score").unwrap(),
                    FieldPath::parse("body").unwrap(),
                ])
                .unwrap(),
            ),
            PageOptions::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(diagnostics.stats(ObjectKind::Record).reads, records);
    assert_eq!(
        diagnostics.stats(ObjectKind::Projection).reads - projections,
        1
    );
    assert!(
        before
            .query(
                &query(&definition, -1, Projection::Full),
                PageOptions::default(),
                None
            )
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
#[tokio::test]
async fn maximum_live_indexes_and_a_replacement_are_maintained_without_skipping_generations() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(obelisk_storage::metrics::InstrumentedStorage::new(
        folder(&dir).await,
    ));
    let engine = open(metrics.clone()).await;
    engine
        .commit(&tx("seed", vec![put("people/a", Value::Integer(1))]))
        .await
        .unwrap();
    let mut definitions = vec![];
    let mut handles = vec![];
    for n in 0..MAX_LIVE_INDEXES {
        let definition = definition(&format!("scores-{n:02}"), ScalarType::Integer, false);
        let created = engine
            .create_index(&request(&format!("create-{n}")), definition.clone())
            .await
            .unwrap();
        finish(&engine, &created.handle, &format!("finish-{n}"), 1).await;
        definitions.push(definition);
        handles.push(created.handle);
    }
    let before = engine.snapshot().await.unwrap().reference();
    let counters = metrics.stats();
    assert!(matches!(
        engine
            .create_index(
                &request("over-limit"),
                definition("too-many", ScalarType::Integer, false)
            )
            .await,
        Err(Error::ResourceLimit("live index count"))
    ));
    assert_eq!(metrics.stats().object_creates, counters.object_creates);
    assert_eq!(
        metrics.stats().head_cas_attempts,
        counters.head_cas_attempts
    );
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    let rebuilding = engine
        .rebuild_index(&request("rebuild"), &handles[0])
        .await
        .unwrap();
    let receipt = engine
        .commit(&tx("update-all", vec![put("people/a", Value::Integer(2))]))
        .await
        .unwrap();
    for (definition, handle) in definitions.iter().zip(&handles) {
        let page = engine
            .query(
                &query(definition, 2, Projection::Full),
                PageOptions::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(page.index, *handle);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(
            Some(&page.entries[0].revision),
            receipt.revisions[&path("people/a")].as_ref()
        );
    }
    finish(&engine, &rebuilding.handle, "replacement", 1).await;
    let page = engine
        .query(
            &query(
                &definitions[0],
                2,
                Projection::fields(vec![FieldPath::parse("body").unwrap()]).unwrap(),
            ),
            PageOptions::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.index, rebuilding.handle);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        Some(&page.entries[0].revision),
        receipt.revisions[&path("people/a")].as_ref()
    );
    eprintln!(
        "maximum-index safety: 16 active definitions plus one building replacement maintained in one document publication; 17th definition rejected before uploads"
    );
}
