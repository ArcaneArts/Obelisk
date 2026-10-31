use super::*;
use crate::{
    index_catalog::{prepare_catalog, stage_descriptor},
    index_tests::{definition, finish, folder, open, path, put, request, tx},
    metadata::{ObjectRef, lifecycle::CoveredProjection},
};
use obelisk_storage::ObjectId;

async fn fixture() -> (tempfile::TempDir, Engine, IndexedQuery) {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(folder(&dir).await).await;
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
    finish(&engine, &created.handle, "finish", 1).await;
    let query = IndexedQuery::new(
        definition.scope().clone(),
        vec![],
        None,
        definition.fields().to_vec(),
        Projection::fields(vec![FieldPath::parse("body").unwrap()]).unwrap(),
    )
    .unwrap();
    (dir, engine, query)
}

#[tokio::test]
async fn native_indexed_cursor_versions_generations_and_positions_are_checked() {
    let (_dir, engine, query) = fixture().await;
    let cursor = engine
        .query(&query, PageOptions::new(1).unwrap(), None)
        .await
        .unwrap()
        .next
        .unwrap();
    let mut invalid = cursor.clone();
    invalid.version += 1;
    assert!(matches!(
        engine
            .query(&query, PageOptions::default(), Some(&invalid))
            .await,
        Err(Error::InvalidCursor(_))
    ));
    let mut invalid = cursor.clone();
    invalid.index = IndexHandle::new(
        cursor.index.id().clone(),
        IndexGeneration::new("another-generation").unwrap(),
    );
    assert!(matches!(
        engine
            .query(&query, PageOptions::default(), Some(&invalid))
            .await,
        Err(Error::InvalidCursor(_))
    ));
    let definition = definition("scores", ScalarType::Integer, false);
    let unscoped = IndexDefinition::new(
        definition.id().clone(),
        IndexScope::Descendants(QueryScope::Root),
        definition.fields().to_vec(),
        vec![],
    )
    .unwrap();
    for key in [
        vec![],
        vec![255],
        metadata::index_entry_key(&definition, &[Value::Integer(1)], &path("people/missing"))
            .unwrap(),
        metadata::index_entry_key(&unscoped, &[Value::Integer(1)], &path("other/a")).unwrap(),
    ] {
        let mut invalid = cursor.clone();
        invalid.last_key = key;
        assert!(matches!(
            engine
                .query(&query, PageOptions::default(), Some(&invalid))
                .await,
            Err(Error::InvalidCursor(_))
        ));
    }
    let restricted = IndexedQuery::new(
        definition.scope().clone(),
        vec![],
        Some(ScalarRange::new(Bound::Included(Value::Integer(2)), Bound::Unbounded).unwrap()),
        definition.fields().to_vec(),
        query.projection().clone(),
    )
    .unwrap();
    let mut invalid = cursor;
    invalid.identity =
        indexed_plan::identity(&restricted, &mut engine.options.resources.budget()).unwrap();
    assert!(matches!(
        engine
            .query(&restricted, PageOptions::default(), Some(&invalid))
            .await,
        Err(Error::InvalidCursor(_))
    ));
}

#[tokio::test]
async fn referenced_covered_projection_identity_and_membership_corruption_fail_reads() {
    for fault in [
        "generation",
        "path",
        "revision",
        "fields",
        "missing",
        "version",
        "location",
    ] {
        let (_dir, engine, query) = fixture().await;
        let base = engine.snapshot().await.unwrap();
        let mut budget = base.resources.budget();
        let mut index = base
            .load_index(&IndexId::new("scores").unwrap(), &mut budget)
            .await
            .unwrap()
            .unwrap();
        let tree = index.descriptor.active.as_mut().unwrap();
        let rows = metadata::range(
            base.store.as_ref(),
            Family::ScalarIndexes,
            tree.entries.as_ref(),
            KeyRange {
                start: Bound::Unbounded,
                end: None,
            },
            1,
            &mut budget,
        )
        .await
        .unwrap();
        let (key, value) = &rows[0];
        let mut entry = lifecycle::decode_index_entry(value, &mut budget.decoded).unwrap();
        let mut projection = CoveredProjection {
            generation: entry.generation.clone(),
            path: path("people/a"),
            revision: entry.location.revision.clone(),
            document: Document::new([(
                FieldPath::parse("body").unwrap(),
                Value::Binary(vec![7; 20_000]),
            )])
            .unwrap(),
        };
        match fault {
            "generation" => {
                projection.generation = IndexGeneration::new("wrong-generation").unwrap()
            }
            "path" => projection.path = path("people/wrong"),
            "revision" => projection.revision = Revision::new("wrong-revision").unwrap(),
            "fields" => {
                projection.document =
                    Document::new([(FieldPath::parse("uncovered").unwrap(), Value::Boolean(true))])
                        .unwrap()
            }
            "location" => entry.location.revision = Revision::new("wrong-revision").unwrap(),
            _ => {}
        }
        let mut bytes = lifecycle::encode_projection(&projection).unwrap();
        if fault == "version" {
            bytes[5..7].copy_from_slice(&2_u16.to_le_bytes());
        }
        let object = ObjectId::random();
        entry.projection = Some(ObjectRef::new(object, &bytes));
        let mut objects = if fault == "missing" {
            vec![]
        } else {
            vec![(object, bytes.into())]
        };
        let changes = metadata::Changes::from([(
            key.clone(),
            Some(lifecycle::encode_index_entry(&entry).unwrap()),
        )]);
        let prepared = metadata::prepare(
            base.store.as_ref(),
            Family::ScalarIndexes,
            tree.entries.as_ref(),
            &changes,
            &mut budget,
        )
        .await
        .unwrap();
        tree.entries = prepared.root;
        objects.extend(prepared.objects);
        let mut root = base.manifest.clone();
        root.sequence += 1;
        root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let value = stage_descriptor(&index.descriptor, &mut objects, &mut budget).unwrap();
        let changes = metadata::Changes::from([(
            metadata::index_definition_key(index.definition.id()),
            Some(value),
        )]);
        prepare_catalog(&base, &mut root, &changes, &mut objects, &mut budget)
            .await
            .unwrap();
        engine
            .publish_root(&base, &root, objects, &mut budget, &mut false)
            .await
            .unwrap();
        let reopened = open(base.store.clone()).await;
        let error = reopened
            .query(&query, PageOptions::default(), None)
            .await
            .unwrap_err();
        if fault == "version" {
            assert!(matches!(error, Error::UnsupportedVersion(2)), "{error:?}");
        } else {
            assert!(matches!(error, Error::Corrupt(_)), "{fault}: {error:?}");
        }
    }
}
