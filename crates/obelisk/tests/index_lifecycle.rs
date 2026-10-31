#![cfg(all(feature = "folder", unix))]
use obelisk::*;

#[tokio::test]
async fn facade_lifecycle_reopens_and_replays_receipts_before_state_conditions() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::open_folder(dir.path()).await.unwrap();
    let document =
        Document::new([(FieldPath::parse("score").unwrap(), Value::Integer(3))]).unwrap();
    database
        .put("seed", "people/a", document.clone(), Condition::Any)
        .await
        .unwrap();
    let definition = IndexDefinition::new(
        IndexId::new("scores").unwrap(),
        IndexScope::Children(QueryScope::path("people").unwrap()),
        vec![IndexField::new(
            FieldPath::parse("score").unwrap(),
            ScalarType::Integer,
            IndexOrder::Ascending,
        )],
        vec![],
    )
    .unwrap();
    let created = database
        .create_index("create", definition.clone())
        .await
        .unwrap();
    assert!(
        database
            .get_index(definition.id())
            .await
            .unwrap()
            .unwrap()
            .active
            .is_none()
    );
    assert_eq!(database.list_indexes().await.unwrap().len(), 1);
    let active = database
        .advance_index_build("step", &created.handle, IndexBuildOptions::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(active.state, IndexState::Active);
    let pinned = database.snapshot().await.unwrap();
    drop(database);
    let database = Database::open_folder(dir.path()).await.unwrap();
    assert_eq!(
        database
            .create_index("create", definition.clone())
            .await
            .unwrap(),
        created
    );
    assert_eq!(
        database.resolve_index_operation("step").await.unwrap(),
        Some(active.clone())
    );
    assert_eq!(
        database
            .get_index(definition.id())
            .await
            .unwrap()
            .unwrap()
            .active,
        Some(active.handle.clone())
    );
    let rebuild = database
        .rebuild_index("rebuild", &active.handle)
        .await
        .unwrap();
    database
        .put("during-rebuild", "people/b", document, Condition::Any)
        .await
        .unwrap();
    let rebuilt = database
        .advance_index_build(
            "rebuild-step",
            &rebuild.handle,
            IndexBuildOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(rebuilt.state, IndexState::Active);
    database.drop_index("drop", &rebuilt.handle).await.unwrap();
    assert!(database.list_indexes().await.unwrap().is_empty());
    assert_eq!(
        pinned
            .get_index(definition.id())
            .await
            .unwrap()
            .unwrap()
            .active,
        Some(active.handle)
    );
    assert!(matches!(
        database.resolve("create").await,
        Err(Error::RequestKindMismatch(_))
    ));
}
