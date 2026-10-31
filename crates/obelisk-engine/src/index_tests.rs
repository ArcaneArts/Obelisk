#![cfg(unix)]
use crate::{
    diagnostics::{InstrumentedMetadata, ObjectKind},
    metadata::{
        self, Family, KeyRange,
        lifecycle::{self, TaggedOutcome},
    },
    *,
};
use obelisk_storage::{ObjectId, Storage};
use obelisk_types::*;
use std::{ops::Bound, sync::Arc};

pub(super) fn path(s: &str) -> DocumentPath {
    DocumentPath::parse(s).unwrap()
}
pub(super) fn request(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}
fn field(s: &str) -> FieldPath {
    FieldPath::parse(s).unwrap()
}
pub(super) fn definition(id: &str, scalar_type: ScalarType, children: bool) -> IndexDefinition {
    let scope = QueryScope::path("people").unwrap();
    IndexDefinition::new(
        IndexId::new(id).unwrap(),
        if children {
            IndexScope::Children(scope)
        } else {
            IndexScope::Descendants(scope)
        },
        vec![IndexField::new(
            field("score"),
            scalar_type,
            IndexOrder::Ascending,
        )],
        vec![field("body")],
    )
    .unwrap()
}
fn document(score: Value) -> Document {
    Document::new([
        (field("score"), score),
        (field("body"), Value::Binary(vec![7; 20_000])),
    ])
    .unwrap()
}
pub(super) fn put(path: &str, score: Value) -> Mutation {
    Mutation {
        path: self::path(path),
        condition: Condition::Any,
        kind: MutationKind::Put(document(score)),
    }
}
pub(super) fn tx(id: &str, mutations: Vec<Mutation>) -> Transaction {
    Transaction::new(request(id), mutations, vec![]).unwrap()
}
pub(super) async fn open(store: Arc<dyn Storage>) -> Engine {
    Engine::open(store, EngineOptions::default()).await.unwrap()
}
pub(super) async fn folder(dir: &tempfile::TempDir) -> Arc<dyn Storage> {
    Arc::new(
        obelisk_storage::folder::Folder::open(dir.path())
            .await
            .unwrap(),
    )
}
pub(super) async fn finish(
    engine: &Engine,
    handle: &IndexHandle,
    label: &str,
    count: usize,
) -> IndexReceipt {
    for i in 0..100 {
        let receipt = engine
            .advance_index_build(
                &request(&format!("{label}-{i}")),
                handle,
                IndexBuildOptions::new(count).unwrap(),
            )
            .await
            .unwrap();
        if receipt.state == IndexState::Active {
            return receipt;
        }
        assert_eq!(receipt.state, IndexState::Building);
    }
    panic!("bounded fixture build did not finish");
}
pub(super) async fn assert_rows(snapshot: &Snapshot, id: &IndexId, expected: &[(&str, i64)]) {
    let mut budget = snapshot.resources.budget();
    let index = snapshot.load_index(id, &mut budget).await.unwrap().unwrap();
    let active = index.descriptor.active.as_ref().expect("active");
    let rows = metadata::range(
        snapshot.store.as_ref(),
        Family::ScalarIndexes,
        active.entries.as_ref(),
        KeyRange {
            start: Bound::Unbounded,
            end: None,
        },
        256,
        &mut budget,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), expected.len());
    for ((key, value), (expected_path, score)) in rows.into_iter().zip(expected) {
        let (values, path) = metadata::decode_index_entry_key(&index.definition, &key).unwrap();
        assert_eq!(path, self::path(expected_path));
        assert_eq!(values, vec![Value::Integer(*score)]);
        let entry = lifecycle::decode_index_entry(&value, &mut budget.decoded).unwrap();
        assert_eq!(entry.generation, active.generation);
        let location = snapshot
            .location(&path, &mut budget)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.location, location);
        let projection = entry.projection.expect("covered projection");
        let bytes = projection
            .read(
                snapshot.store.as_ref(),
                lifecycle::MAX_PROJECTION_BYTES,
                &mut budget,
            )
            .await
            .unwrap();
        let covered = lifecycle::decode_projection(&bytes, &mut budget.decoded).unwrap();
        assert_eq!(covered.path, path);
        assert_eq!(covered.generation, active.generation);
        assert_eq!(covered.revision, location.revision);
        let original = snapshot
            .read_location(&path, location, &mut budget)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            covered.document,
            Document::new([(
                field("body"),
                original.document.get(&field("body")).unwrap().clone()
            )])
            .unwrap()
        );
    }
}

#[tokio::test]
async fn incremental_build_reconciles_current_documents_and_reopens_with_original_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    let seed = tx(
        "seed",
        vec![
            put("people/a", Value::Integer(1)),
            put("people/b", Value::Integer(2)),
            put("people/c", Value::Integer(3)),
            put("people/d", Value::Integer(4)),
            put("people", Value::Integer(999)),
            put("people/d/leaf", Value::Integer(888)),
            put("elsewhere", Value::Integer(777)),
        ],
    );
    let original = engine.commit(&seed).await.unwrap();
    let definition = definition("scores", ScalarType::Integer, true);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    assert!(
        engine
            .get_index(definition.id())
            .await
            .unwrap()
            .unwrap()
            .active
            .is_none()
    );
    let first = engine
        .advance_index_build(
            &request("first"),
            &created.handle,
            IndexBuildOptions::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.state, IndexState::Building);
    assert_eq!(first.progress.last_path(), Some(&path("people/a")));
    engine
        .commit(&tx(
            "during-build",
            vec![
                put("people/a", Value::Integer(9)),
                put("people/aa", Value::Integer(0)),
                put("people/e", Value::Integer(-1)),
                put("people/c", Value::String("type change".into())),
                Mutation {
                    path: path("people/b"),
                    condition: Condition::Any,
                    kind: MutationKind::Delete,
                },
                Mutation {
                    path: path("people/d"),
                    condition: Condition::Any,
                    kind: MutationKind::Patch(
                        Patch::new([(field("body"), Value::Binary(vec![8; 24_000]))], vec![])
                            .unwrap(),
                    ),
                },
            ],
        ))
        .await
        .unwrap();
    let reopened = open(store.clone()).await;
    let second = reopened
        .advance_index_build(
            &request("second"),
            &created.handle,
            IndexBuildOptions::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.progress.examined(), 2);
    assert_eq!(second.progress.last_path(), Some(&path("people/b")));
    let active = finish(&reopened, &created.handle, "finish", 1).await;
    assert_eq!(active.progress.examined(), 4); // Source paths, not current rows or emitted entries.
    let pinned = reopened.snapshot().await.unwrap();
    assert_rows(
        &pinned,
        definition.id(),
        &[
            ("people/e", -1),
            ("people/aa", 0),
            ("people/d", 4),
            ("people/a", 9),
        ],
    )
    .await;
    assert_eq!(
        reopened
            .create_index(&request("create"), definition.clone())
            .await
            .unwrap(),
        created
    );
    assert_eq!(
        reopened
            .advance_index_build(
                &request("first"),
                &created.handle,
                IndexBuildOptions::new(1).unwrap()
            )
            .await
            .unwrap(),
        first
    );
    assert_eq!(reopened.commit(&seed).await.unwrap(), original);
    assert!(matches!(
        reopened.resolve(&request("create")).await,
        Err(Error::RequestKindMismatch(_))
    ));
    assert!(matches!(
        reopened.resolve_index_operation(&request("seed")).await,
        Err(Error::RequestKindMismatch(_))
    ));
    assert!(matches!(
        reopened
            .commit(&tx("create", vec![put("other", Value::Integer(5))]))
            .await,
        Err(Error::RequestIdReuse(_))
    ));
    let dropped = reopened
        .drop_index(&request("drop"), &active.handle)
        .await
        .unwrap();
    assert_eq!(dropped.state, IndexState::Dropped);
    assert!(reopened.list_indexes().await.unwrap().is_empty());
    let replacement = reopened
        .create_index(&request("recreate"), definition.clone())
        .await
        .unwrap();
    assert_ne!(replacement.handle, active.handle);
    assert!(matches!(
        reopened
            .drop_index(&request("stale-drop"), &active.handle)
            .await,
        Err(Error::IndexStateConflict(_))
    ));
    assert_eq!(
        reopened
            .drop_index(&request("drop"), &active.handle)
            .await
            .unwrap(),
        dropped
    );
    assert_eq!(
        reopened
            .resolve_index_operation(&request("create"))
            .await
            .unwrap(),
        Some(created)
    );
    let historical = reopened.snapshot_at(pinned.reference()).await.unwrap();
    assert_rows(
        &historical,
        definition.id(),
        &[
            ("people/e", -1),
            ("people/aa", 0),
            ("people/d", 4),
            ("people/a", 9),
        ],
    )
    .await;
}

#[tokio::test]
async fn rebuild_maintains_both_generations_and_preserves_historical_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    engine
        .commit(&tx(
            "seed",
            vec![
                put("people/a", Value::Integer(1)),
                put("people/b", Value::Integer(2)),
            ],
        ))
        .await
        .unwrap();
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    let old = finish(&engine, &created.handle, "initial", 1).await;
    let historical = engine.snapshot().await.unwrap();
    let rebuild = engine
        .rebuild_index(&request("rebuild"), &old.handle)
        .await
        .unwrap();
    engine
        .advance_index_build(
            &request("rebuild-first"),
            &rebuild.handle,
            IndexBuildOptions::new(1).unwrap(),
        )
        .await
        .unwrap();
    engine
        .commit(&tx(
            "change",
            vec![
                put("people/a", Value::Integer(8)),
                put("people/b/leaf", Value::Integer(0)),
            ],
        ))
        .await
        .unwrap();
    let status = engine.get_index(definition.id()).await.unwrap().unwrap();
    assert_eq!(status.active, Some(old.handle.clone()));
    assert!(status.candidate.is_some());
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/b/leaf", 0), ("people/b", 2), ("people/a", 8)],
    )
    .await;
    let rebuilt = finish(&open(store).await, &rebuild.handle, "rebuild-rest", 1).await;
    assert_ne!(rebuilt.handle, old.handle);
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/b/leaf", 0), ("people/b", 2), ("people/a", 8)],
    )
    .await;
    assert_rows(
        &historical,
        definition.id(),
        &[("people/a", 1), ("people/b", 2)],
    )
    .await;
    assert_eq!(
        engine
            .rebuild_index(&request("rebuild"), &old.handle)
            .await
            .unwrap(),
        rebuild
    );
    assert!(matches!(
        engine
            .rebuild_index(&request("stale-rebuild"), &old.handle)
            .await,
        Err(Error::IndexStateConflict(_))
    ));
}

#[tokio::test]
async fn oversized_source_failure_is_persisted_and_old_unindexable_values_can_be_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(folder(&dir).await).await;
    engine
        .commit(&tx(
            "seed",
            vec![put("people/a", Value::String("x".repeat(5000)))],
        ))
        .await
        .unwrap();
    let definition = definition("names", ScalarType::String, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        engine
            .commit(&tx(
                "invalid-new",
                vec![put("people/a", Value::String("y".repeat(5000)))]
            ))
            .await,
        Err(Error::ResourceLimit("scalar index key bytes"))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    assert!(
        engine
            .resolve(&request("invalid-new"))
            .await
            .unwrap()
            .is_none()
    );
    let failed = engine
        .advance_index_build(
            &request("fail-build"),
            &created.handle,
            IndexBuildOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(failed.state, IndexState::Failed);
    assert_eq!(failed.progress.examined(), 0);
    assert_eq!(
        failed.failure,
        Some(IndexBuildFailure::ScalarKeyTooLarge(path("people/a")))
    );
    engine
        .commit(&tx(
            "allowed-after-failure",
            vec![put("people/a", Value::String("z".repeat(5000)))],
        ))
        .await
        .unwrap();
    engine
        .commit(&tx(
            "repair",
            vec![put("people/a", Value::String("short".into()))],
        ))
        .await
        .unwrap();
    let rebuild = engine
        .rebuild_index(&request("retry-build"), &failed.handle)
        .await
        .unwrap();
    let active = finish(&engine, &rebuild.handle, "finish", 1).await;
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        engine
            .commit(&tx(
                "invalid-active",
                vec![
                    put("people/a", Value::String("q".repeat(5000))),
                    put("people/never", Value::String("valid".into()))
                ]
            ))
            .await,
        Err(Error::ResourceLimit("scalar index key bytes"))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    assert!(engine.get(&path("people/never")).await.unwrap().is_none());
    assert_eq!(
        engine
            .get_index(definition.id())
            .await
            .unwrap()
            .unwrap()
            .active,
        Some(active.handle)
    );
    // Repair before the base scan must also work: the oversized old key had no candidate entry.
    engine
        .drop_index(&request("drop"), &rebuild.handle)
        .await
        .unwrap();
    engine
        .commit(&tx(
            "large-again",
            vec![put("people/a", Value::String("r".repeat(5000)))],
        ))
        .await
        .unwrap();
    let created = engine
        .create_index(&request("create-again"), definition)
        .await
        .unwrap();
    engine
        .commit(&tx(
            "repair-before-scan",
            vec![put("people/a", Value::String("fixed".into()))],
        ))
        .await
        .unwrap();
    finish(&engine, &created.handle, "last", 1).await;
}

#[tokio::test]
async fn scope_eligibility_precedes_scalar_key_size_validation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(folder(&dir).await).await;
    let definition = IndexDefinition::new(
        IndexId::new("composite").unwrap(),
        IndexScope::Descendants(QueryScope::Root),
        vec![
            IndexField::new(field("score"), ScalarType::String, IndexOrder::Ascending),
            IndexField::new(
                field("required"),
                ScalarType::Boolean,
                IndexOrder::Descending,
            ),
        ],
        vec![],
    )
    .unwrap();
    engine
        .commit(&tx(
            "seed",
            vec![put("missing-second", Value::String("x".repeat(5000)))],
        ))
        .await
        .unwrap();
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    finish(&engine, &created.handle, "finish", 1).await;
    engine
        .commit(&tx(
            "still-omitted",
            vec![put("missing-second", Value::String("y".repeat(5000)))],
        ))
        .await
        .unwrap();
    let snapshot = engine.snapshot().await.unwrap();
    let index = snapshot
        .load_index(definition.id(), &mut snapshot.resources.budget())
        .await
        .unwrap()
        .unwrap();
    assert!(index.descriptor.active.unwrap().entries.is_none());
}

#[tokio::test]
async fn source_byte_limits_and_aggregate_limits_never_skip_unprocessed_input_or_publish_failures()
{
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    engine
        .commit(&tx(
            "seed",
            vec![
                put("people/a", Value::Integer(1)),
                put("people/b", Value::Integer(2)),
                put("people/c", Value::Integer(3)),
            ],
        ))
        .await
        .unwrap();
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    let snapshot = engine.snapshot().await.unwrap();
    let location = snapshot
        .location(&path("people/a"), &mut snapshot.resources.budget())
        .await
        .unwrap()
        .unwrap();
    let cost = (location.range.end - location.range.start) as usize;
    let before = snapshot.reference();
    assert!(matches!(
        engine
            .advance_index_build(
                &request("too-small"),
                &created.handle,
                IndexBuildOptions::default()
                    .with_source_byte_limit(cost - 1)
                    .unwrap()
            )
            .await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    let diagnostics = Arc::new(InstrumentedMetadata::new(store.clone()));
    let measured = open(diagnostics.clone()).await;
    let step = measured
        .advance_index_build(
            &request("one-byte-page"),
            &created.handle,
            IndexBuildOptions::default()
                .with_source_byte_limit(cost)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(step.progress.last_path(), Some(&path("people/a")));
    assert_eq!(step.progress.examined(), 1);
    assert_eq!(diagnostics.stats(ObjectKind::Record).reads, 1);
    let low = Engine::open(
        store.clone(),
        EngineOptions {
            resources: ResourceLimits {
                metadata_page_reads: 1,
                ..ResourceLimits::default()
            },
            ..EngineOptions::default()
        },
    )
    .await
    .unwrap();
    let metrics = Arc::new(obelisk_storage::metrics::InstrumentedStorage::new(store));
    let limited = Engine::open(metrics.clone(), low.options.clone())
        .await
        .unwrap();
    let before = engine.snapshot().await.unwrap().reference();
    let initial = metrics.stats();
    assert!(matches!(
        limited
            .advance_index_build(
                &request("resource-fail"),
                &created.handle,
                IndexBuildOptions::default()
            )
            .await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(metrics.stats().object_creates, initial.object_creates);
    assert_eq!(metrics.stats().head_cas_attempts, initial.head_cas_attempts);
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    finish(&engine, &created.handle, "rest", 1).await;
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/a", 1), ("people/b", 2), ("people/c", 3)],
    )
    .await;
}

#[tokio::test]
async fn limits_reject_extra_builds_and_low_writer_limits_but_preserve_request_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    let seed = tx("seed", vec![put("people/a", Value::Integer(1))]);
    let original = engine.commit(&seed).await.unwrap();
    let first = engine
        .create_index(
            &request("first"),
            definition("first", ScalarType::Integer, false),
        )
        .await
        .unwrap();
    assert!(matches!(
        engine
            .create_index(
                &request("second"),
                definition("second", ScalarType::Integer, true)
            )
            .await,
        Err(Error::IndexBuildInProgress)
    ));
    finish(&engine, &first.handle, "finish-first", 1).await;
    let second = engine
        .create_index(
            &request("second"),
            definition("second", ScalarType::Integer, true),
        )
        .await
        .unwrap();
    let limited = Engine::open(
        store,
        EngineOptions {
            indexes: IndexLimits::new(1).unwrap(),
            ..EngineOptions::default()
        },
    )
    .await
    .unwrap();
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        limited
            .commit(&tx("blocked", vec![put("people/a", Value::Integer(2))]))
            .await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    assert_eq!(limited.commit(&seed).await.unwrap(), original);
    finish(&engine, &second.handle, "finish-second", 1).await;
}

#[tokio::test]
async fn v2_opening_fails_without_writes_and_v3_startup_keeps_index_data_lazy() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let root = metadata::Root {
        database_id: DatabaseId::new("old").unwrap(),
        commit_id: CommitId::new("old-commit").unwrap(),
        sequence: 0,
        documents: None,
        requests: None,
        membership: None,
    };
    let bytes = metadata::encode_root(&root).unwrap();
    let id = ObjectId::random();
    let head = metadata::HeadRef {
        database_id: root.database_id,
        commit_id: root.commit_id,
        sequence: 0,
        root: metadata::ObjectRef::new(id, &bytes),
    };
    store.create(id, bytes.into()).await.unwrap();
    store
        .compare_exchange_head(None, metadata::encode_head(&head).unwrap().into())
        .await
        .unwrap();
    let metrics = Arc::new(obelisk_storage::metrics::InstrumentedStorage::new(store));
    assert!(matches!(
        Engine::open(metrics.clone(), EngineOptions::default()).await,
        Err(Error::UnsupportedVersion(2))
    ));
    assert_eq!(metrics.stats().object_creates, 0);
    assert_eq!(metrics.stats().head_cas_attempts, 0);
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    let created = engine
        .create_index(
            &request("create"),
            definition("scores", ScalarType::Integer, false),
        )
        .await
        .unwrap();
    finish(&engine, &created.handle, "finish", 1).await;
    let diagnostics = Arc::new(InstrumentedMetadata::new(store));
    open(diagnostics.clone()).await;
    assert_eq!(diagnostics.stats(ObjectKind::Root).reads, 1);
    for kind in [
        ObjectKind::IndexDefinitions,
        ObjectKind::ScalarIndexes,
        ObjectKind::DefinitionObject,
        ObjectKind::IndexDescriptor,
        ObjectKind::Projection,
        ObjectKind::Documents,
        ObjectKind::Requests,
        ObjectKind::Record,
    ] {
        assert_eq!(diagnostics.stats(kind).reads, 0);
    }
    let mut budget = engine.options.resources.budget();
    let snapshot = engine.snapshot().await.unwrap();
    let reference = snapshot
        .outcome_ref(&request("create"), &mut budget)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot
            .request_outcome(&request("create"), &reference, &mut budget)
            .await
            .unwrap(),
        TaggedOutcome::Index { .. }
    ));
}
