//! Indexed process qualification shares the backend, gates and fault classifier.
use super::*;
use std::collections::BTreeMap;
fn field(text: &str) -> FieldPath {
    FieldPath::parse(text).unwrap()
}
fn definition() -> IndexDefinition {
    IndexDefinition::new(
        IndexId::new("scores").unwrap(),
        IndexScope::Descendants(QueryScope::path("people").unwrap()),
        vec![IndexField::new(
            field("value"),
            ScalarType::Integer,
            IndexOrder::Descending,
        )],
        vec![field("cover"), field("tags")],
    )
    .unwrap()
}
fn indexed_document(value: i64) -> Document {
    Document::new([
        (field("value"), Value::Integer(value)),
        (field("cover"), Value::Binary(vec![value as u8; 256])),
        (
            field("tags"),
            Value::List(TypedList::new(ValueType::Boolean, vec![]).unwrap()),
        ),
    ])
    .unwrap()
}
fn batch(request: &str, paths: Vec<DocumentPath>, value: i64) -> Transaction {
    Transaction::new(
        RequestId::new(request).unwrap(),
        paths
            .into_iter()
            .map(|path| Mutation {
                path,
                condition: Condition::Any,
                kind: MutationKind::Put(indexed_document(value)),
            })
            .collect(),
        vec![],
    )
    .unwrap()
}
fn crash_write() -> Transaction {
    batch(
        "index-crash-write",
        vec![path("people/crash/a"), path("people/crash/b")],
        77,
    )
}
fn query(covered: bool) -> IndexedQuery {
    let definition = definition();
    IndexedQuery::new(
        definition.scope().clone(),
        vec![],
        None,
        definition.fields().to_vec(),
        if covered {
            Projection::fields(vec![field("value"), field("cover"), field("tags")]).unwrap()
        } else {
            Projection::Full
        },
    )
    .unwrap()
}
async fn active(db: &Database) -> IndexHandle {
    db.get_index(definition().id())
        .await
        .unwrap()
        .unwrap()
        .active
        .unwrap()
}
async fn finish(db: &Database, label: &str) {
    for step in 0..1000 {
        let status = db.get_index(definition().id()).await.unwrap().unwrap();
        let Some(candidate) = status.candidate else {
            return;
        };
        assert_eq!(candidate.state, IndexState::Building);
        let receipt = db
            .advance_index_build(
                &format!("{label}-{step}"),
                &candidate.handle,
                IndexBuildOptions::new(32).unwrap(),
            )
            .await
            .unwrap();
        if receipt.state == IndexState::Active {
            return;
        }
    }
    panic!("index build did not finish");
}
async fn management(db: &Database, mode: &str, handle: Option<&IndexHandle>) -> IndexReceipt {
    match mode {
        "index-create-crash" => db.create_index("index-crash", definition()).await.unwrap(),
        "index-advance-crash" | "index-activate-crash" => db
            .advance_index_build(
                "index-crash",
                handle.unwrap(),
                IndexBuildOptions::new(if mode == "index-activate-crash" {
                    256
                } else {
                    1
                })
                .unwrap(),
            )
            .await
            .unwrap(),
        "index-rebuild-crash" => db
            .rebuild_index("index-crash", handle.unwrap())
            .await
            .unwrap(),
        "index-drop-crash" => db.drop_index("index-crash", handle.unwrap()).await.unwrap(),
        _ => panic!("unknown indexed management mode"),
    }
}
pub(super) async fn worker(mode: &str, db: &Database, fault: &FaultStore, writer: usize) {
    let phase = &fault.stage;
    if mode == "index-builder" {
        for step in 0..1000 {
            let Some(status) = db.get_index(definition().id()).await.unwrap() else {
                return;
            };
            let Some(candidate) = status.candidate else {
                return;
            };
            match db
                .advance_index_build(
                    &format!("builder-{phase}-{writer}-{step}"),
                    &candidate.handle,
                    IndexBuildOptions::new(1).unwrap(),
                )
                .await
            {
                Ok(receipt) => {
                    if step == 0 {
                        gate(
                            &fault.ready,
                            &fault.release.with_file_name("builders-continue"),
                            "first-chunk",
                        );
                    }
                    if receipt.state == IndexState::Active {
                        return;
                    }
                    assert_eq!(receipt.state, IndexState::Building);
                }
                Err(Error::IndexStateConflict(_) | Error::IndexNotFound(_)) => return,
                other => panic!("builder failed: {other:?}"),
            }
        }
        panic!("builder did not finish");
    } else if mode == "index-writer" {
        for step in 0..4 {
            let paths: Vec<_> = [
                format!("people/writer-{writer}/a"),
                format!("people/writer-{writer}/b"),
            ]
            .into_iter()
            .map(|p| path(&p))
            .collect();
            let request = format!("writer-{phase}-{writer}-{step}");
            let transaction = if step == 2 {
                Transaction::new(
                    RequestId::new(request).unwrap(),
                    paths
                        .into_iter()
                        .map(|path| Mutation {
                            path,
                            condition: Condition::Any,
                            kind: MutationKind::Delete,
                        })
                        .collect(),
                    vec![],
                )
                .unwrap()
            } else {
                batch(&request, paths, (writer * 10 + step) as i64)
            };
            db.commit(&transaction).await.unwrap();
            if step == 0 {
                gate(
                    &fault.ready,
                    &fault.release.with_file_name("writers-continue"),
                    "first-pair",
                );
            }
        }
    } else if mode == "index-stale-builder" {
        let handle = db
            .get_index(definition().id())
            .await
            .unwrap()
            .unwrap()
            .candidate
            .unwrap()
            .handle;
        fault.armed.store(true, Ordering::SeqCst);
        assert!(matches!(
            db.advance_index_build("stale-step", &handle, IndexBuildOptions::new(1).unwrap())
                .await,
            Err(Error::IndexStateConflict(_))
        ));
        assert!(
            db.resolve_index_operation("stale-step")
                .await
                .unwrap()
                .is_none()
        );
    } else if mode == "index-drop" {
        db.drop_index(&format!("independent-drop-{phase}"), &active(db).await)
            .await
            .unwrap();
    } else {
        let handle = if mode == "index-create-crash" {
            None
        } else {
            let status = db.get_index(definition().id()).await.unwrap().unwrap();
            Some(
                if mode == "index-advance-crash" || mode == "index-activate-crash" {
                    status.candidate.unwrap().handle
                } else {
                    status.active.unwrap()
                },
            )
        };
        fault.armed.store(true, Ordering::SeqCst);
        let commit = if mode == "index-write-crash" {
            db.commit(&crash_write()).await.unwrap().commit_id
        } else {
            management(db, mode, handle.as_ref())
                .await
                .publication
                .commit_id
        };
        if phase == "after-ack" {
            gate(&fault.ready, &fault.release, &format!("ack:{commit}"));
        }
        panic!("indexed fault stage was not reached: {mode} {phase}");
    }
}

async fn check(snapshot: &Snapshot) -> Vec<QueryEntry> {
    let scope = QueryScope::path("people").unwrap();
    let mut expected = snapshot_query(snapshot, &scope, false).await;
    expected.retain(|entry| matches!(entry.document.get(&field("value")), Some(Value::Integer(_))));
    expected.sort_by(|a, b| {
        let value = |entry: &QueryEntry| match entry.document.get(&field("value")).unwrap() {
            Value::Integer(v) => *v,
            _ => unreachable!(),
        };
        value(b)
            .cmp(&value(a))
            .then_with(|| a.path.segments().cmp(b.path.segments()))
    });
    let status = snapshot.get_index(definition().id()).await.unwrap();
    if status.as_ref().is_none_or(|status| status.active.is_none()) {
        assert!(matches!(
            snapshot
                .query(&query(false), PageOptions::default(), None)
                .await,
            Err(Error::IndexUnavailable { .. })
        ));
        return expected;
    }
    for covered in [false, true] {
        let mut rows = vec![];
        let mut cursor = None;
        loop {
            let page = snapshot
                .query(
                    &query(covered),
                    PageOptions::new(5).unwrap(),
                    cursor.as_ref(),
                )
                .await
                .unwrap();
            rows.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(rows, expected, "torn indexed/documents/projection snapshot");
    }
    expected
}
async fn concurrent(backend: &Backend, signals: &Path, phase: &str, db: &Database) {
    let signals = signals.join(phase);
    std::fs::create_dir(&signals).unwrap();
    let release = signals.join("start-builders");
    let mut builders: Vec<_> = (0..4)
        .map(|writer| {
            Worker::spawn(
                backend,
                &signals,
                "index-builder",
                writer,
                phase,
                true,
                &release,
            )
        })
        .collect();
    for worker in &mut builders {
        assert_eq!(worker.marker(), "ready");
    }
    std::fs::write(&release, "go").unwrap();
    for worker in &mut builders {
        worker.wait_label("first-chunk");
    }
    let release = signals.join("start-writers");
    let mut writers: Vec<_> = (0..4)
        .map(|writer| {
            Worker::spawn(
                backend,
                &signals,
                "index-writer",
                writer,
                phase,
                true,
                &release,
            )
        })
        .collect();
    for worker in &mut writers {
        assert_eq!(worker.marker(), "ready");
    }
    std::fs::write(&release, "go").unwrap();
    for worker in &mut writers {
        worker.wait_label("first-pair");
    }
    let paused = db.snapshot().await.unwrap();
    assert!(
        paused
            .get_index(definition().id())
            .await
            .unwrap()
            .unwrap()
            .candidate
            .is_some()
    );
    check(&paused).await;
    std::fs::write(signals.join("builders-continue"), "go").unwrap();
    std::fs::write(signals.join("writers-continue"), "go").unwrap();
    let mut scans = 0;
    let started = Instant::now();
    loop {
        let snapshot = db.snapshot().await.unwrap();
        let rows = check(&snapshot).await;
        let map: BTreeMap<_, _> = rows.iter().map(|r| (r.path.clone(), &r.document)).collect();
        for writer in 0..4 {
            assert_eq!(
                map.get(&path(&format!("people/writer-{writer}/a"))),
                map.get(&path(&format!("people/writer-{writer}/b")))
            );
        }
        scans += 1;
        if builders.iter_mut().all(Worker::exited) && writers.iter_mut().all(Worker::exited) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "indexed concurrent phase timed out"
        );
    }
    for worker in builders.iter_mut().chain(&mut writers) {
        worker.finish();
    }
    let snapshot = backend.open().await.snapshot().await.unwrap();
    assert!(
        snapshot
            .get_index(definition().id())
            .await
            .unwrap()
            .unwrap()
            .candidate
            .is_none()
    );
    check(&snapshot).await;
    eprintln!(
        "indexed multiprocess {phase}: 4 builders, 4 writers/16 atomic pair mutations, {scans} pinned reference scans; current/covered results coherent"
    );
}

async fn crash_cases(backend: &Backend, signals: &Path) {
    let cases: &[(&str, &[&str])] = &[
        (
            "index-create-crash",
            &[
                "before-definition",
                "after-definition",
                "before-descriptor",
                "after-descriptor",
                "after-catalog-page",
                "after-manifest",
                "before-publication",
                "after-publication",
                "after-ack",
            ],
        ),
        (
            "index-advance-crash",
            &[
                "before-projection",
                "after-projection",
                "before-scalar-page",
                "after-scalar-page",
                "before-descriptor",
                "after-descriptor",
                "before-catalog-page",
                "after-catalog-page",
                "after-outcome",
                "before-manifest",
                "after-manifest",
                "before-publication",
                "after-publication",
                "after-ack",
            ],
        ),
        (
            "index-activate-crash",
            &[
                "after-projection",
                "after-scalar-page",
                "after-descriptor",
                "after-manifest",
                "before-publication",
                "after-publication",
                "after-ack",
            ],
        ),
        (
            "index-rebuild-crash",
            &[
                "after-descriptor",
                "after-catalog-page",
                "after-manifest",
                "after-publication",
                "after-ack",
            ],
        ),
        (
            "index-drop-crash",
            &[
                "after-request-page",
                "after-manifest",
                "before-publication",
                "after-publication",
                "after-ack",
            ],
        ),
        (
            "index-write-crash",
            &[
                "after-projection",
                "after-scalar-page",
                "after-descriptor",
                "after-document-page",
                "after-membership-page",
                "after-catalog-page",
                "after-manifest",
                "before-publication",
                "after-publication",
                "after-ack",
            ],
        ),
    ];
    let mut reached = 0;
    for (mode, stages) in cases {
        for stage in *stages {
            let backend = backend.isolated(&format!("{mode}-{stage}"));
            let db = backend.open().await;
            let seed = db
                .commit(&batch(
                    "indexed-seed",
                    (0..3).map(|n| path(&format!("people/base/{n}"))).collect(),
                    1,
                ))
                .await
                .unwrap();
            let handle = if *mode == "index-create-crash" {
                None
            } else {
                let created = db
                    .create_index("indexed-create", definition())
                    .await
                    .unwrap();
                if *mode == "index-advance-crash" || *mode == "index-activate-crash" {
                    Some(created.handle)
                } else {
                    finish(&db, "indexed-initial-build").await;
                    if *mode == "index-drop-crash" || *mode == "index-write-crash" {
                        db.rebuild_index("indexed-pending-rebuild", &created.handle)
                            .await
                            .unwrap();
                    }
                    Some(created.handle)
                }
            };
            let before = db.snapshot().await.unwrap();
            let expected = check(&before).await;
            let release = signals.join(format!("{mode}-{stage}.release"));
            let mut worker = Worker::spawn(&backend, signals, mode, 0, stage, false, &release);
            let marker = worker.marker();
            worker.kill();
            reached += 1;
            let reopened = backend.open().await;
            let snapshot = reopened.snapshot().await.unwrap();
            let published = *stage == "after-publication" || *stage == "after-ack";
            if *stage != "after-ack" {
                assert_eq!(marker, *stage, "wrong indexed boundary reached");
            }
            assert_eq!(
                snapshot.sequence(),
                before.sequence() + u64::from(published)
            );
            assert_eq!(reopened.resolve("indexed-seed").await.unwrap(), Some(seed));
            let outcome = if *mode == "index-write-crash" {
                let original = reopened.resolve("index-crash-write").await.unwrap();
                assert_eq!(original.is_some(), published);
                if let Some(original) = &original
                    && *stage == "after-ack"
                {
                    assert_eq!(marker, format!("ack:{}", original.commit_id));
                }
                let receipt = reopened.commit(&crash_write()).await.unwrap();
                if let Some(original) = original {
                    assert_eq!(receipt, original);
                }
                receipt.commit_id
            } else {
                let original = reopened
                    .resolve_index_operation("index-crash")
                    .await
                    .unwrap();
                assert_eq!(original.is_some(), published);
                if let Some(original) = &original
                    && *stage == "after-ack"
                {
                    assert_eq!(marker, format!("ack:{}", original.publication.commit_id));
                }
                let receipt = management(&reopened, mode, handle.as_ref()).await;
                if let Some(original) = original {
                    assert_eq!(receipt, original);
                }
                receipt.publication.commit_id
            };
            assert_eq!(
                reopened.snapshot().await.unwrap().sequence(),
                before.sequence() + 1
            );
            let after = reopened.snapshot().await.unwrap();
            if *mode == "index-drop-crash" {
                assert!(after.get_index(definition().id()).await.unwrap().is_none());
            } else {
                finish(&reopened, "indexed-resume").await;
                check(&reopened.snapshot().await.unwrap()).await;
            }
            assert_eq!(
                check(&before).await,
                expected,
                "retained pre-crash snapshot changed"
            );
            assert!(!outcome.as_str().is_empty());
            eprintln!(
                "indexed process kill: {mode} {stage}; marker reached, coherent state, original receipt, resumable progress and retained snapshot"
            );
        }
    }
    assert_eq!(reached, 50);
    eprintln!(
        "indexed process kills: {reached} explicitly reached boundaries; these are client process kills, not storage-server or hardware power loss"
    );
}

pub(super) async fn suite(backend: &Backend, signals: &Path) {
    let backend = backend.isolated("indexed-concurrency");
    let db = backend.open().await;
    db.commit(&batch(
        "indexed-seed",
        (0..32)
            .map(|n| path(&format!("people/base/{n:03}")))
            .collect(),
        7,
    ))
    .await
    .unwrap();
    let created = db
        .create_index("indexed-create", definition())
        .await
        .unwrap();
    concurrent(&backend, signals, "index-build", &db).await;
    assert_eq!(active(&db).await, created.handle);
    let first = db
        .query(&query(false), PageOptions::new(1).unwrap(), None)
        .await
        .unwrap();
    let pinned = db.snapshot_at(first.snapshot.clone()).await.unwrap();
    let expected = check(&pinned).await;
    let rebuild = db
        .rebuild_index("indexed-rebuild", &created.handle)
        .await
        .unwrap();
    concurrent(&backend, signals, "index-rebuild", &db).await;
    assert_eq!(active(&db).await, rebuild.handle);
    db.rebuild_index("indexed-drop-race", &rebuild.handle)
        .await
        .unwrap();
    let stale_release = signals.join("stale-builder.release");
    let mut stale = Worker::spawn(
        &backend,
        signals,
        "index-stale-builder",
        0,
        "before-publication",
        false,
        &stale_release,
    );
    assert_eq!(stale.marker(), "before-publication");
    let release = signals.join("index-drop.release");
    let mut dropper = Worker::spawn(&backend, signals, "index-drop", 0, "drop", false, &release);
    dropper.finish();
    let recreated = db
        .create_index("indexed-recreate", definition())
        .await
        .unwrap();
    std::fs::write(&stale_release, "go").unwrap();
    stale.finish();
    let status = db.get_index(definition().id()).await.unwrap().unwrap();
    let candidate = status.candidate.unwrap();
    assert_eq!(candidate.handle, recreated.handle);
    assert_eq!(candidate.progress.examined(), 0);
    finish(&db, "indexed-recreated-build").await;
    let mut dropper = Worker::spawn(
        &backend,
        signals,
        "index-drop",
        0,
        "drop-recreated",
        false,
        &release,
    );
    dropper.finish();
    eprintln!(
        "indexed multiprocess stale rebuild: prepared chunk lost CAS to independent drop/recreation; replacement progress stayed untouched"
    );
    let reopened = backend.open().await;
    assert!(
        reopened
            .get_index(definition().id())
            .await
            .unwrap()
            .is_none()
    );
    let mut rows = first.entries;
    let mut cursor = first.next;
    while let Some(position) = cursor {
        let page = reopened
            .query(&query(false), PageOptions::new(7).unwrap(), Some(&position))
            .await
            .unwrap();
        rows.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(rows, expected);
    eprintln!(
        "indexed multiprocess retained pagination: original cursor survived independent writes, rebuild, drop and reopen"
    );
    crash_cases(&backend, signals).await;
}
