#![cfg(unix)]
use crate::{
    index_catalog::{Objects, prepare_catalog, stage_descriptor},
    index_tests::{assert_rows, definition, finish, folder, open, path, put, request, tx},
    metadata::{
        self, Family, KeyRange,
        lifecycle::{self, IndexDescriptor},
    },
    *,
};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::{ops::Bound, sync::Arc};

async fn publish_descriptor(
    engine: &Engine,
    base: &Snapshot,
    descriptor: &IndexDescriptor,
    mut objects: Objects,
) {
    let mut budget = base.resources.budget();
    let mut root = base.manifest.clone();
    root.sequence += 1;
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let value = stage_descriptor(descriptor, &mut objects, &mut budget).unwrap();
    let changes =
        metadata::Changes::from([(metadata::index_definition_key(&descriptor.id), Some(value))]);
    prepare_catalog(base, &mut root, &changes, &mut objects, &mut budget)
        .await
        .unwrap();
    engine
        .publish_root(base, &root, objects, &mut budget, &mut false)
        .await
        .unwrap();
}

#[tokio::test]
async fn cross_object_definition_and_entry_identities_are_checked_before_writes() {
    for invalid_definition in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = folder(&dir).await;
        let engine = open(store.clone()).await;
        engine
            .commit(&tx("seed", vec![put("people/a", Value::Integer(1))]))
            .await
            .unwrap();
        let definition = definition("scores", ScalarType::Integer, false);
        let created = engine
            .create_index(&request("create"), definition.clone())
            .await
            .unwrap();
        finish(&engine, &created.handle, "finish", 1).await;
        let base = engine.snapshot().await.unwrap();
        let mut budget = base.resources.budget();
        let mut index = base
            .load_index(definition.id(), &mut budget)
            .await
            .unwrap()
            .unwrap();
        let mut objects = Vec::new();
        if invalid_definition {
            let other = super::index_tests::definition("another-id", ScalarType::Integer, false);
            let bytes = metadata::encode_index_definition(&other).unwrap();
            let id = ObjectId::random();
            index.descriptor.definition = metadata::ObjectRef::new(id, &bytes);
            objects.push((id, bytes.into()));
        } else {
            let active = index.descriptor.active.as_mut().unwrap();
            let rows = metadata::range(
                store.as_ref(),
                Family::ScalarIndexes,
                active.entries.as_ref(),
                KeyRange {
                    start: Bound::Unbounded,
                    end: None,
                },
                2,
                &mut budget,
            )
            .await
            .unwrap();
            let (key, value) = &rows[0];
            let mut entry = lifecycle::decode_index_entry(value, &mut budget.decoded).unwrap();
            entry.generation = IndexGeneration::new("another-generation").unwrap();
            let changes = metadata::Changes::from([(
                key.clone(),
                Some(lifecycle::encode_index_entry(&entry).unwrap()),
            )]);
            let prepared = metadata::prepare(
                store.as_ref(),
                Family::ScalarIndexes,
                active.entries.as_ref(),
                &changes,
                &mut budget,
            )
            .await
            .unwrap();
            active.entries = prepared.root;
            objects.extend(prepared.objects);
        }
        publish_descriptor(&engine, &base, &index.descriptor, objects).await;
        // Opening remains lazy even when a referenced definition/entry is corrupt.
        let reopened = open(store).await;
        let before = reopened.snapshot().await.unwrap().reference();
        if invalid_definition {
            assert!(matches!(
                reopened.get_index(definition.id()).await,
                Err(Error::Corrupt(_))
            ));
        }
        assert!(matches!(
            reopened
                .commit(&tx("must-fail", vec![put("people/a", Value::Integer(2))]))
                .await,
            Err(Error::Corrupt(_))
        ));
        assert_eq!(reopened.snapshot().await.unwrap().reference(), before);
        assert!(
            reopened
                .resolve(&request("must-fail"))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn build_progress_must_identify_a_real_pinned_source_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(folder(&dir).await).await;
    engine
        .commit(&tx(
            "seed",
            vec![
                put("people/a", Value::Integer(1)),
                put("people/z", Value::Integer(2)),
            ],
        ))
        .await
        .unwrap();
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    let base = engine.snapshot().await.unwrap();
    let mut budget = base.resources.budget();
    let mut index = base
        .load_index(definition.id(), &mut budget)
        .await
        .unwrap()
        .unwrap();
    index.descriptor.candidate.as_mut().unwrap().progress =
        IndexBuildProgress::new(1, Some(path("people/missing"))).unwrap();
    publish_descriptor(&engine, &base, &index.descriptor, vec![]).await;
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        engine
            .advance_index_build(
                &request("bad-position"),
                &created.handle,
                IndexBuildOptions::default()
            )
            .await,
        Err(Error::Corrupt(_))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
}

#[tokio::test]
async fn metadata_only_publications_preserve_catalog_entries_revisions_and_original_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(folder(&dir).await).await;
    let seed = tx(
        "seed",
        vec![
            put("people/a", Value::Integer(1)),
            put("people/b", Value::Integer(2)),
        ],
    );
    let original = engine.commit(&seed).await.unwrap();
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    finish(&engine, &created.handle, "finish", 1).await;
    let base = engine.snapshot().await.unwrap();
    let old_catalog = base.manifest.catalog.clone();
    let mut root = base.manifest.clone();
    root.sequence += 1;
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    engine
        .publish_root(
            &base,
            &root,
            vec![],
            &mut base.resources.budget(),
            &mut false,
        )
        .await
        .unwrap();
    assert_eq!(
        engine.snapshot().await.unwrap().manifest.catalog,
        old_catalog
    );
    assert_eq!(engine.commit(&seed).await.unwrap(), original);
    assert_eq!(
        engine
            .resolve_index_operation(&request("create"))
            .await
            .unwrap(),
        Some(created)
    );
    engine
        .commit(&tx("newer-write", vec![put("people/a", Value::Integer(3))]))
        .await
        .unwrap();
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(matches!(
        engine
            .publish_root(
                &base,
                &root,
                vec![],
                &mut base.resources.budget(),
                &mut false
            )
            .await,
        Err(Error::Storage(obelisk_storage::Error::Conflict))
    ));
    let fresh = engine.snapshot().await.unwrap();
    let mut root = fresh.manifest.clone();
    root.sequence += 1;
    root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
    engine
        .publish_root(
            &fresh,
            &root,
            vec![],
            &mut fresh.resources.budget(),
            &mut false,
        )
        .await
        .unwrap();
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/b", 2), ("people/a", 3)],
    )
    .await;
    assert_rows(&base, definition.id(), &[("people/a", 1), ("people/b", 2)]).await;
}

#[tokio::test]
async fn covered_projection_preparation_budget_fails_without_uploads_or_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    engine
        .commit(&tx("seed", vec![put("people/a", Value::Integer(1))]))
        .await
        .unwrap();
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    finish(&engine, &created.handle, "finish", 1).await;
    let metrics = Arc::new(obelisk_storage::metrics::InstrumentedStorage::new(store));
    let limited = Engine::open(
        metrics.clone(),
        EngineOptions {
            resources: ResourceLimits {
                prepared_metadata_bytes: 32 * 1024,
                ..ResourceLimits::default()
            },
            ..EngineOptions::default()
        },
    )
    .await
    .unwrap();
    let before = engine.snapshot().await.unwrap().reference();
    let counters = metrics.stats();
    let document = Document::new([
        (FieldPath::parse("score").unwrap(), Value::Integer(2)),
        (
            FieldPath::parse("body").unwrap(),
            Value::Binary(vec![3; 64 * 1024]),
        ),
    ])
    .unwrap();
    let transaction = tx(
        "oversized-projection-work",
        vec![Mutation {
            path: path("people/a"),
            condition: Condition::Any,
            kind: MutationKind::Put(document),
        }],
    );
    assert!(matches!(
        limited.commit(&transaction).await,
        Err(Error::ResourceLimit("prepared covered projection bytes"))
    ));
    assert_eq!(metrics.stats().object_creates, counters.object_creates);
    assert_eq!(
        metrics.stats().head_cas_attempts,
        counters.head_cas_attempts
    );
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/a", 1)],
    )
    .await;
}

#[tokio::test]
async fn initialization_must_fit_the_configured_snapshot_reader_before_upload() {
    for resources in [
        ResourceLimits {
            metadata_bytes: 1,
            ..ResourceLimits::default()
        },
        ResourceLimits {
            decoded_bytes: 900,
            ..ResourceLimits::default()
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let metrics = Arc::new(obelisk_storage::metrics::InstrumentedStorage::new(
            folder(&dir).await,
        ));
        assert!(matches!(
            Engine::open(
                metrics.clone(),
                EngineOptions {
                    resources,
                    ..EngineOptions::default()
                }
            )
            .await,
            Err(Error::ResourceLimit(_))
        ));
        assert_eq!(metrics.stats().object_creates, 0);
        assert_eq!(metrics.stats().head_cas_attempts, 0);
    }
}
