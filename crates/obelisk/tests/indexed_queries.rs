#![cfg(all(feature = "folder", unix))]
use obelisk::*;
use std::{cmp::Ordering, collections::BTreeMap, ops::Bound, sync::Arc};
fn field(p: &str) -> FieldPath {
    FieldPath::parse(p).unwrap()
}
fn ordered(p: &str, kind: ScalarType, order: IndexOrder) -> IndexField {
    IndexField::new(field(p), kind, order)
}
fn equality(p: &str, value: Value) -> EqualityPredicate {
    EqualityPredicate::new(field(p), value).unwrap()
}
fn scope() -> IndexScope {
    IndexScope::Descendants(QueryScope::path("people").unwrap())
}
fn definition(id: &str, fields: Vec<IndexField>, covers: Vec<FieldPath>) -> IndexDefinition {
    IndexDefinition::new(IndexId::new(id).unwrap(), scope(), fields, covers).unwrap()
}
async fn build(db: &Database, definition: IndexDefinition) -> IndexHandle {
    let id = definition.id().as_str().to_owned();
    let created = db
        .create_index(&format!("create-{id}"), definition)
        .await
        .unwrap();
    for n in 0..100 {
        let receipt = db
            .advance_index_build(
                &format!("build-{id}-{n}"),
                &created.handle,
                IndexBuildOptions::new(7).unwrap(),
            )
            .await
            .unwrap();
        assert!(receipt.progress.examined() <= (n + 1) * 7);
        if receipt.state == IndexState::Active {
            return receipt.handle;
        }
    }
    panic!("build did not finish");
}
fn compare(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (Value::Double(a), Value::Double(b)) => a.get().total_cmp(&b.get()),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        _ => panic!("reference compared different scalar types"),
    }
}
fn reference(
    model: &BTreeMap<DocumentPath, VersionedDocument>,
    query: &IndexedQuery,
) -> Vec<QueryEntry> {
    let mut selected: Vec<_> = model
        .iter()
        .filter(|(path, v)| {
            if !query.requirement().scope.contains(path) {
                return false;
            }
            if query
                .equality()
                .iter()
                .any(|p| v.document.get(p.field()) != Some(p.value()))
            {
                return false;
            }
            if query.requirement().order.iter().any(|p| {
                v.document
                    .get(p.path())
                    .is_none_or(|v| !p.scalar_type().matches(v))
            }) {
                return false;
            }
            if let Some(range) = query.range() {
                let value = v.document.get(query.requirement().order[0].path()).unwrap();
                let lower = match range.lower() {
                    Bound::Included(v) => compare(value, v) != Ordering::Less,
                    Bound::Excluded(v) => compare(value, v) == Ordering::Greater,
                    Bound::Unbounded => true,
                };
                let upper = match range.upper() {
                    Bound::Included(v) => compare(value, v) != Ordering::Greater,
                    Bound::Excluded(v) => compare(value, v) == Ordering::Less,
                    Bound::Unbounded => true,
                };
                if !lower || !upper {
                    return false;
                }
            }
            true
        })
        .collect();
    selected.sort_by(|(left_path, left), (right_path, right)| {
        for p in &query.requirement().order {
            let mut compared = compare(
                left.document.get(p.path()).unwrap(),
                right.document.get(p.path()).unwrap(),
            );
            if p.order() == IndexOrder::Descending {
                compared = compared.reverse();
            }
            if compared != Ordering::Equal {
                return compared;
            }
        }
        left_path.segments().cmp(right_path.segments())
    });
    selected
        .into_iter()
        .map(|(path, v)| QueryEntry {
            path: path.clone(),
            revision: v.revision.clone(),
            document: match query.projection() {
                Projection::Full => v.document.clone(),
                Projection::Fields(fields) => Document::new(
                    fields
                        .iter()
                        .filter_map(|f| v.document.get(f).map(|v| (f.clone(), v.clone()))),
                )
                .unwrap(),
            },
        })
        .collect()
}
async fn all(db: &Database, query: &IndexedQuery, options: PageOptions) -> Vec<QueryEntry> {
    let mut rows = Vec::new();
    let mut cursor = None;
    let mut snapshot = None;
    loop {
        let page = db.query(query, options, cursor.as_ref()).await.unwrap();
        if let Some(snapshot) = &snapshot {
            assert_eq!(snapshot, &page.snapshot);
        } else {
            snapshot = Some(page.snapshot.clone());
        }
        assert!(page.entries.len() <= options.limit());
        rows.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return rows;
        }
        assert!(rows.len() < 1000, "pagination did not terminate");
    }
}
async fn seed(db: &Database) -> BTreeMap<DocumentPath, VersionedDocument> {
    let names = ["", "a", "a\0x", "z", "ß", "á", "\0"];
    let mut paths = Vec::new();
    let mut mutations = Vec::new();
    for n in 0..140 {
        let path = DocumentPath::from_segments(if n % 4 == 0 {
            vec![
                "people".into(),
                "missing/parent".into(),
                format!("item{n:03}"),
            ]
        } else {
            vec!["people".into(), format!("item{n:03}")]
        })
        .unwrap();
        let mut fields = vec![
            (
                field("group"),
                Value::String(if n % 2 == 0 { "a\0b" } else { "other" }.into()),
            ),
            (field("name"), Value::String(names[n % names.len()].into())),
            (field("enabled"), Value::Boolean(n % 3 == 0)),
            (
                field("rating"),
                Value::double((n as f64 - 60.0) / 7.0).unwrap(),
            ),
            (field("body"), Value::Binary(vec![n as u8; 128])),
            (
                field("tags"),
                Value::List(TypedList::new(ValueType::String, vec![]).unwrap()),
            ),
        ];
        if n % 11 != 0 {
            fields.push((
                field("score"),
                if n % 13 == 0 {
                    Value::double(1.0).unwrap()
                } else {
                    Value::Integer(n as i64 % 9 - 4)
                },
            ));
        }
        paths.push(path.clone());
        mutations.push(Mutation {
            path,
            condition: Condition::Any,
            kind: MutationKind::Put(Document::new(fields).unwrap()),
        });
    }
    for text in ["people", "people-other/a", "outside/item"] {
        let path = DocumentPath::parse(text).unwrap();
        paths.push(path.clone());
        mutations.push(Mutation {
            path,
            condition: Condition::Any,
            kind: MutationKind::Put(Document::new([(field("score"), Value::Integer(0))]).unwrap()),
        });
    }
    db.commit(&Transaction::new(RequestId::new("seed").unwrap(), mutations, vec![]).unwrap())
        .await
        .unwrap();
    let snapshot = db.snapshot().await.unwrap();
    let mut model = BTreeMap::new();
    for path in paths {
        model.insert(path.clone(), snapshot.get(&path).await.unwrap().unwrap());
    }
    model
}

#[tokio::test]
async fn paginated_mixed_scalar_queries_match_independent_document_reference() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    let model = seed(&db).await;
    for direction in [IndexOrder::Ascending, IndexOrder::Descending] {
        let fields = vec![
            ordered("group", ScalarType::String, IndexOrder::Descending),
            ordered("score", ScalarType::Integer, direction),
            ordered("name", ScalarType::String, IndexOrder::Ascending),
        ];
        let id = if direction == IndexOrder::Ascending {
            "scores-asc"
        } else {
            "scores-desc"
        };
        build(
            &db,
            definition(id, fields.clone(), vec![field("body"), field("tags")]),
        )
        .await;
        for bounds in [
            (
                Bound::Included(Value::Integer(-2)),
                Bound::Included(Value::Integer(3)),
            ),
            (
                Bound::Excluded(Value::Integer(-2)),
                Bound::Excluded(Value::Integer(3)),
            ),
            (Bound::Unbounded, Bound::Excluded(Value::Integer(0))),
            (Bound::Included(Value::Integer(2)), Bound::Unbounded),
            (
                Bound::Included(Value::Integer(9)),
                Bound::Included(Value::Integer(-9)),
            ),
            (
                Bound::Excluded(Value::Integer(1)),
                Bound::Included(Value::Integer(1)),
            ),
        ] {
            for projection in [
                Projection::Full,
                Projection::fields(vec![
                    field("name"),
                    field("score"),
                    field("body"),
                    field("tags"),
                    field("absent"),
                ])
                .unwrap(),
            ] {
                let query = IndexedQuery::new(
                    scope(),
                    vec![equality("group", Value::String("a\0b".into()))],
                    Some(ScalarRange::new(bounds.0.clone(), bounds.1.clone()).unwrap()),
                    fields[1..].to_vec(),
                    projection,
                )
                .unwrap()
                .with_index(IndexId::new(id).unwrap());
                assert_eq!(
                    all(&db, &query, PageOptions::new(3).unwrap()).await,
                    reference(&model, &query)
                );
            }
        }
        let query = IndexedQuery::new(scope(), vec![], None, fields.clone(), Projection::Full)
            .unwrap()
            .with_index(IndexId::new(id).unwrap());
        assert_eq!(
            all(&db, &query, PageOptions::new(17).unwrap()).await,
            reference(&model, &query)
        );
        let exact = IndexedQuery::new(
            scope(),
            vec![
                equality("group", Value::String("a\0b".into())),
                equality("score", Value::Integer(0)),
                equality("name", Value::String("a".into())),
            ],
            None,
            vec![],
            Projection::Full,
        )
        .unwrap()
        .with_index(IndexId::new(id).unwrap());
        assert_eq!(
            all(&db, &exact, PageOptions::new(1).unwrap()).await,
            reference(&model, &exact)
        );
        let strings = IndexedQuery::new(
            scope(),
            vec![
                equality("group", Value::String("a\0b".into())),
                equality("score", Value::Integer(0)),
            ],
            Some(
                ScalarRange::new(
                    Bound::Excluded(Value::String("a".into())),
                    Bound::Included(Value::String("á".into())),
                )
                .unwrap(),
            ),
            fields[2..].to_vec(),
            Projection::Full,
        )
        .unwrap()
        .with_index(IndexId::new(id).unwrap());
        assert_eq!(
            all(&db, &strings, PageOptions::new(2).unwrap()).await,
            reference(&model, &strings)
        );
    }
    let fields = vec![
        ordered("enabled", ScalarType::Boolean, IndexOrder::Descending),
        ordered("rating", ScalarType::Double, IndexOrder::Descending),
        ordered("name", ScalarType::String, IndexOrder::Descending),
    ];
    build(&db, definition("ratings", fields.clone(), vec![])).await;
    let query = IndexedQuery::new(
        scope(),
        vec![equality("enabled", Value::Boolean(true))],
        Some(
            ScalarRange::new(
                Bound::Excluded(Value::double(-3.0).unwrap()),
                Bound::Included(Value::double(5.0).unwrap()),
            )
            .unwrap(),
        ),
        fields[1..].to_vec(),
        Projection::Full,
    )
    .unwrap();
    assert_eq!(
        all(&db, &query, PageOptions::new(4).unwrap()).await,
        reference(&model, &query)
    );
    for (id, selected_scope) in [
        ("root-children", IndexScope::Children(QueryScope::Root)),
        (
            "root-descendants",
            IndexScope::Descendants(QueryScope::Root),
        ),
        (
            "people-children",
            IndexScope::Children(QueryScope::path("people").unwrap()),
        ),
    ] {
        let order = vec![ordered(
            "score",
            ScalarType::Integer,
            IndexOrder::Descending,
        )];
        build(
            &db,
            IndexDefinition::new(
                IndexId::new(id).unwrap(),
                selected_scope.clone(),
                order.clone(),
                vec![],
            )
            .unwrap(),
        )
        .await;
        let query =
            IndexedQuery::new(selected_scope, vec![], None, order, Projection::Full).unwrap();
        assert_eq!(
            all(&db, &query, PageOptions::new(7).unwrap()).await,
            reference(&model, &query)
        );
    }
}

#[tokio::test]
async fn covering_projection_and_bounded_pages_avoid_full_payload_lookahead() {
    use diagnostics::{InstrumentedMetadata, ObjectKind};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(InstrumentedMetadata::new(Arc::new(
        storage::folder::Folder::open(dir.path()).await.unwrap(),
    )));
    let db = Database::open_with_storage(store.clone(), DatabaseOptions::default())
        .await
        .unwrap();
    let mut model = BTreeMap::new();
    for n in 0..3 {
        let text = format!("people/{n}");
        let document = Document::new([
            (field("score"), Value::Integer(n)),
            (field("body"), Value::Binary(vec![n as u8; 20 * 1024])),
            (
                field("tags"),
                Value::List(
                    TypedList::new(ValueType::List(Box::new(ValueType::String)), vec![]).unwrap(),
                ),
            ),
            (field("secret"), Value::String("payload only".into())),
        ])
        .unwrap();
        db.put(&format!("seed-{n}"), &text, document, Condition::Any)
            .await
            .unwrap();
        model.insert(
            DocumentPath::parse(&text).unwrap(),
            db.get(&text).await.unwrap().unwrap(),
        );
    }
    let order = vec![ordered("score", ScalarType::Integer, IndexOrder::Ascending)];
    build(
        &db,
        definition(
            "scores",
            order.clone(),
            vec![field("body"), field("tags"), field("absent")],
        ),
    )
    .await;
    db.patch(
        "covered-only",
        "people/1",
        Patch::new([(field("body"), Value::Binary(vec![9; 20 * 1024]))], vec![]).unwrap(),
        Condition::Any,
    )
    .await
    .unwrap();
    model.insert(
        DocumentPath::parse("people/1").unwrap(),
        db.get("people/1").await.unwrap().unwrap(),
    );
    let query =
        |projection| IndexedQuery::new(scope(), vec![], None, order.clone(), projection).unwrap();
    for projection in [
        Projection::fields(vec![field("score")]).unwrap(),
        Projection::fields(vec![]).unwrap(),
    ] {
        let records = store.stats(ObjectKind::Record).reads;
        let projections = store.stats(ObjectKind::Projection).reads;
        let query = query(projection);
        assert_eq!(
            all(&db, &query, PageOptions::new(1).unwrap()).await,
            reference(&model, &query)
        );
        assert_eq!(store.stats(ObjectKind::Record).reads, records);
        assert_eq!(store.stats(ObjectKind::Projection).reads, projections);
    }
    let covered = query(
        Projection::fields(vec![
            field("score"),
            field("body"),
            field("tags"),
            field("absent"),
        ])
        .unwrap(),
    );
    let records = store.stats(ObjectKind::Record).reads;
    assert_eq!(
        all(
            &db,
            &covered,
            PageOptions::new(100)
                .unwrap()
                .with_byte_limit(21 * 1024)
                .unwrap()
        )
        .await,
        reference(&model, &covered)
    );
    assert_eq!(store.stats(ObjectKind::Record).reads, records);
    let too_small = query(Projection::Full);
    assert!(matches!(
        db.query(
            &too_small,
            PageOptions::default().with_byte_limit(1).unwrap(),
            None
        )
        .await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(store.stats(ObjectKind::Record).reads, records);
    let full = db
        .query(&too_small, PageOptions::new(1).unwrap(), None)
        .await
        .unwrap();
    assert!(full.next.is_some());
    assert_eq!(full.entries.len(), 1);
    assert_eq!(store.stats(ObjectKind::Record).reads - records, 1);
    let fallback = query(Projection::fields(vec![field("score"), field("secret")]).unwrap());
    let records = store.stats(ObjectKind::Record).reads;
    assert_eq!(
        all(&db, &fallback, PageOptions::new(2).unwrap()).await,
        reference(&model, &fallback)
    );
    assert_eq!(store.stats(ObjectKind::Record).reads - records, 3);
    let records = store.stats(ObjectKind::Record).reads;
    assert_eq!(
        all(
            &db,
            &fallback,
            PageOptions::new(100).unwrap().with_byte_limit(256).unwrap()
        )
        .await,
        reference(&model, &fallback)
    );
    assert_eq!(
        store.stats(ObjectKind::Record).reads - records,
        3,
        "byte-limited fallback must not read a full payload for lookahead"
    );
    let limited = Database::open_with_storage(
        store.clone(),
        DatabaseOptions {
            resources: ResourceLimits {
                decoded_bytes: 8192,
                ..ResourceLimits::default()
            },
            ..DatabaseOptions::default()
        },
    )
    .await
    .unwrap();
    let records = store.stats(ObjectKind::Record).reads;
    assert!(matches!(
        limited.query(&covered, PageOptions::default(), None).await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(store.stats(ObjectKind::Record).reads, records);
}

#[tokio::test]
async fn exact_planning_and_pinned_cursors_survive_writes_reopen_rebuild_and_drop() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    for n in 0..4 {
        db.put(
            &format!("seed-{n}"),
            &format!("people/{n}"),
            Document::new([
                (field("score"), Value::Integer(n)),
                (field("extra"), Value::String("exists".into())),
            ])
            .unwrap(),
            Condition::Any,
        )
        .await
        .unwrap();
    }
    let order = vec![ordered("score", ScalarType::Integer, IndexOrder::Ascending)];
    let query = IndexedQuery::new(scope(), vec![], None, order.clone(), Projection::Full).unwrap();
    let mut extra = order.clone();
    extra.push(ordered("extra", ScalarType::String, IndexOrder::Ascending));
    build(&db, definition("extra", extra, vec![])).await;
    match db.query(&query, PageOptions::default(), None).await {
        Err(Error::IndexUnavailable { required, .. }) => {
            assert_eq!(*required, query.requirement().clone())
        }
        other => panic!("extra unused key must not be selected: {other:?}"),
    }
    let created = db
        .create_index("create-z", definition("z-scores", order.clone(), vec![]))
        .await
        .unwrap();
    assert!(matches!(
        db.query(&query, PageOptions::default(), None).await,
        Err(Error::IndexUnavailable {
            reason: "matching index has no active generation",
            ..
        })
    ));
    db.advance_index_build("finish-z", &created.handle, IndexBuildOptions::default())
        .await
        .unwrap();
    let handle = build(&db, definition("a-scores", order.clone(), vec![])).await;
    let page = db
        .query(&query, PageOptions::new(1).unwrap(), None)
        .await
        .unwrap();
    assert_eq!(page.index, handle); // Deterministic lexical catalog choice.
    let explicit = query.clone().with_index(IndexId::new("z-scores").unwrap());
    assert_eq!(
        db.query(&explicit, PageOptions::default(), None)
            .await
            .unwrap()
            .index,
        created.handle
    );
    let cursor = page.next.unwrap();
    assert!(matches!(
        db.query(&explicit, PageOptions::default(), Some(&cursor))
            .await,
        Err(Error::InvalidCursor(_))
    ));
    let projected = IndexedQuery::new(
        scope(),
        vec![],
        None,
        order.clone(),
        Projection::fields(vec![field("score")]).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        db.query(&projected, PageOptions::default(), Some(&cursor))
            .await,
        Err(Error::InvalidCursor(_))
    ));
    let children = IndexedQuery::new(
        IndexScope::Children(QueryScope::path("people").unwrap()),
        vec![],
        None,
        order.clone(),
        Projection::Full,
    )
    .unwrap();
    assert!(matches!(
        db.query(&children, PageOptions::default(), None).await,
        Err(Error::IndexUnavailable { .. })
    ));
    let wrong_type = IndexedQuery::new(
        scope(),
        vec![],
        None,
        vec![ordered("score", ScalarType::Double, IndexOrder::Ascending)],
        Projection::Full,
    )
    .unwrap();
    assert!(matches!(
        db.query(&wrong_type, PageOptions::default(), None).await,
        Err(Error::IndexUnavailable { .. })
    ));
    let pinned = db.snapshot_at(page.snapshot.clone()).await.unwrap();
    let expected = pinned
        .query(&query, PageOptions::default(), None)
        .await
        .unwrap()
        .entries;
    db.put(
        "modify",
        "people/1",
        Document::new([(field("score"), Value::Integer(100))]).unwrap(),
        Condition::Any,
    )
    .await
    .unwrap();
    db.delete("delete", "people/2", Condition::Any)
        .await
        .unwrap();
    db.put(
        "new",
        "people/new",
        Document::new([(field("score"), Value::Integer(-10))]).unwrap(),
        Condition::Any,
    )
    .await
    .unwrap();
    let mut current_model = BTreeMap::new();
    for text in ["people/0", "people/1", "people/3", "people/new"] {
        current_model.insert(
            DocumentPath::parse(text).unwrap(),
            db.get(text).await.unwrap().unwrap(),
        );
    }
    assert_eq!(
        all(&db, &query, PageOptions::new(2).unwrap()).await,
        reference(&current_model, &query)
    );
    let rebuild = db.rebuild_index("rebuild", &handle).await.unwrap();
    assert_eq!(
        db.query(&query, PageOptions::default(), None)
            .await
            .unwrap()
            .index,
        handle
    );
    db.advance_index_build(
        "finish-rebuild",
        &rebuild.handle,
        IndexBuildOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        all(&db, &query, PageOptions::new(2).unwrap()).await,
        reference(&current_model, &query)
    );
    db.drop_index("drop-a", &rebuild.handle).await.unwrap();
    db.drop_index("drop-z", &created.handle).await.unwrap();
    assert!(matches!(
        db.query(&query, PageOptions::default(), None).await,
        Err(Error::IndexUnavailable { .. })
    ));
    let reopened = Database::open_folder(dir.path()).await.unwrap();
    let mut actual = page.entries;
    let mut cursor = Some(cursor);
    while let Some(position) = cursor {
        let next = reopened
            .query(&query, PageOptions::new(2).unwrap(), Some(&position))
            .await
            .unwrap();
        actual.extend(next.entries);
        cursor = next.next;
    }
    assert_eq!(actual, expected);
    assert_eq!(
        pinned
            .query(&query, PageOptions::default(), None)
            .await
            .unwrap()
            .entries,
        expected
    );
    let original_cursor = pinned
        .query(&query, PageOptions::new(1).unwrap(), None)
        .await
        .unwrap()
        .next
        .unwrap();
    assert!(matches!(
        reopened
            .snapshot()
            .await
            .unwrap()
            .query(&query, PageOptions::default(), Some(&original_cursor))
            .await,
        Err(Error::InvalidCursor(_))
    ));
    let other_dir = tempfile::tempdir().unwrap();
    let other = Database::open_folder(other_dir.path()).await.unwrap();
    assert!(matches!(
        other
            .query(&query, PageOptions::default(), Some(&original_cursor))
            .await,
        Err(Error::InvalidCursor(_))
    ));
}
