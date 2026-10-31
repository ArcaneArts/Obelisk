#![cfg(unix)]
use crate::{
    index_tests::{assert_rows, definition, finish, folder, open, path, put, request, tx},
    *,
};
use obelisk_storage::{Head, ObjectId, Storage, VersionToken};
use obelisk_types::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
struct FaultStore {
    inner: Arc<dyn Storage>,
    gate: AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
    lose: AtomicUsize,
    hide_read: AtomicBool,
    fail_create: AtomicBool,
    conflicts: AtomicUsize,
}
impl FaultStore {
    fn new(inner: Arc<dyn Storage>) -> Self {
        Self {
            inner,
            gate: AtomicBool::new(false),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            lose: AtomicUsize::new(0),
            hide_read: AtomicBool::new(false),
            fail_create: AtomicBool::new(false),
            conflicts: AtomicUsize::new(0),
        }
    }
    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.arrived.notified())
            .await
            .expect("CAS gate not reached");
    }
}
#[async_trait::async_trait]
impl Storage for FaultStore {
    async fn read(
        &self,
        id: ObjectId,
        range: Option<std::ops::Range<u64>>,
    ) -> obelisk_storage::Result<bytes::Bytes> {
        self.inner.read(id, range).await
    }
    async fn create(&self, id: ObjectId, bytes: bytes::Bytes) -> obelisk_storage::Result<()> {
        if self.fail_create.swap(false, Ordering::SeqCst) {
            return Err(obelisk_storage::Error::Transport(
                "injected create failure".into(),
            ));
        }
        self.inner.create(id, bytes).await
    }
    async fn read_head(&self) -> obelisk_storage::Result<Option<Head>> {
        if self.hide_read.swap(false, Ordering::SeqCst) {
            return Err(obelisk_storage::Error::Transport(
                "lost resolution read".into(),
            ));
        }
        self.inner.read_head().await
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: bytes::Bytes,
    ) -> obelisk_storage::Result<VersionToken> {
        if self.gate.swap(false, Ordering::SeqCst) {
            self.arrived.notify_one();
            self.release.notified().await;
        }
        let result = self.inner.compare_exchange_head(expected, bytes).await;
        if matches!(result, Err(obelisk_storage::Error::Conflict)) {
            self.conflicts.fetch_add(1, Ordering::SeqCst);
        }
        let version = result?;
        match self.lose.swap(0, Ordering::SeqCst) {
            1 => Err(obelisk_storage::Error::Transport(
                "lost successful publication response".into(),
            )),
            2 => Err(obelisk_storage::Error::Conflict),
            3 => {
                self.hide_read.store(true, Ordering::SeqCst);
                Err(obelisk_storage::Error::Transport(
                    "lost success and resolution".into(),
                ))
            }
            _ => Ok(version),
        }
    }
    async fn list(&self) -> obelisk_storage::Result<Vec<ObjectId>> {
        self.inner.list().await
    }
    async fn delete(&self, id: ObjectId) -> obelisk_storage::Result<()> {
        self.inner.delete(id).await
    }
}

#[tokio::test]
async fn writer_prepared_before_registration_retries_with_candidate_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    engine
        .commit(&tx("seed", vec![put("people/a", Value::Integer(1))]))
        .await
        .unwrap();
    let gate = Arc::new(FaultStore::new(store));
    let writer = open(gate.clone()).await;
    gate.gate.store(true, Ordering::SeqCst);
    let task = tokio::spawn(async move {
        writer
            .commit(&tx(
                "before-registration",
                vec![put("people/z", Value::Integer(9))],
            ))
            .await
    });
    gate.wait().await;
    let definition = definition("scores", ScalarType::Integer, false);
    let created = engine
        .create_index(&request("create"), definition.clone())
        .await
        .unwrap();
    gate.release.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(gate.conflicts.load(Ordering::SeqCst), 1);
    let active = finish(&engine, &created.handle, "finish", 1).await;
    assert_eq!(active.progress.examined(), 1); // The new path was never part of the pinned source.
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/a", 1), ("people/z", 9)],
    )
    .await;
}

#[tokio::test]
async fn activation_conflicts_and_competing_builders_reconcile_latest_values_and_progress() {
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
    let gate = Arc::new(FaultStore::new(store));
    let builder = open(gate.clone()).await;
    gate.gate.store(true, Ordering::SeqCst);
    let handle = created.handle.clone();
    let task = tokio::spawn(async move {
        builder
            .advance_index_build(
                &request("activation"),
                &handle,
                IndexBuildOptions::default(),
            )
            .await
    });
    gate.wait().await;
    engine
        .commit(&tx(
            "during-activation",
            vec![
                put("people/a", Value::Integer(3)),
                put("people/c", Value::Integer(4)),
            ],
        ))
        .await
        .unwrap();
    gate.release.notify_one();
    let active = task.await.unwrap().unwrap();
    assert_eq!(active.state, IndexState::Active);
    assert_eq!(gate.conflicts.load(Ordering::SeqCst), 1);
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/b", 2), ("people/a", 3), ("people/c", 4)],
    )
    .await;
    let rebuilding = engine
        .rebuild_index(&request("rebuild"), &active.handle)
        .await
        .unwrap();
    let builder = open(gate.clone()).await;
    gate.gate.store(true, Ordering::SeqCst);
    let handle = rebuilding.handle.clone();
    let task = tokio::spawn(async move {
        builder
            .advance_index_build(
                &request("builder-one"),
                &handle,
                IndexBuildOptions::new(1).unwrap(),
            )
            .await
    });
    gate.wait().await;
    let first = engine
        .advance_index_build(
            &request("builder-two"),
            &rebuilding.handle,
            IndexBuildOptions::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.progress.examined(), 1);
    gate.release.notify_one();
    let second = task.await.unwrap().unwrap();
    assert_eq!(second.progress.examined(), 2);
    assert_eq!(second.progress.last_path(), Some(&path("people/b")));
    assert_eq!(gate.conflicts.load(Ordering::SeqCst), 2);
    finish(&engine, &rebuilding.handle, "rest", 1).await;
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/b", 2), ("people/a", 3), ("people/c", 4)],
    )
    .await;
}

#[tokio::test]
async fn a_stale_builder_cannot_publish_after_drop_and_recreation() {
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
    let gate = Arc::new(FaultStore::new(store));
    let builder = open(gate.clone()).await;
    gate.gate.store(true, Ordering::SeqCst);
    let handle = created.handle.clone();
    let task = tokio::spawn(async move {
        builder
            .advance_index_build(
                &request("stale-builder"),
                &handle,
                IndexBuildOptions::default(),
            )
            .await
    });
    gate.wait().await;
    engine
        .drop_index(&request("drop"), &created.handle)
        .await
        .unwrap();
    let replacement = engine
        .create_index(&request("replacement"), definition.clone())
        .await
        .unwrap();
    gate.release.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        Err(Error::IndexStateConflict(_))
    ));
    assert_eq!(gate.conflicts.load(Ordering::SeqCst), 1);
    let status = engine.get_index(definition.id()).await.unwrap().unwrap();
    assert!(status.active.is_none());
    assert_eq!(status.candidate.unwrap().handle, replacement.handle);
    assert!(
        engine
            .resolve_index_operation(&request("stale-builder"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn management_response_loss_resolves_all_operations_and_final_conflict_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let faults = Arc::new(FaultStore::new(store));
    let engine = open(faults.clone()).await;
    let definition = definition("scores", ScalarType::Integer, false);
    faults.lose.store(3, Ordering::SeqCst);
    assert!(matches!(
        engine
            .create_index(&request("create"), definition.clone())
            .await,
        Err(Error::Uncertain { .. })
    ));
    let created = engine
        .resolve_index_operation(&request("create"))
        .await
        .unwrap()
        .unwrap();
    faults.lose.store(3, Ordering::SeqCst);
    assert!(matches!(
        engine
            .advance_index_build(
                &request("advance"),
                &created.handle,
                IndexBuildOptions::default()
            )
            .await,
        Err(Error::Uncertain { .. })
    ));
    let active = engine
        .resolve_index_operation(&request("advance"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.state, IndexState::Active);
    faults.lose.store(3, Ordering::SeqCst);
    assert!(matches!(
        engine
            .rebuild_index(&request("rebuild"), &active.handle)
            .await,
        Err(Error::Uncertain { .. })
    ));
    let rebuilding = engine
        .resolve_index_operation(&request("rebuild"))
        .await
        .unwrap()
        .unwrap();
    faults.lose.store(3, Ordering::SeqCst);
    assert!(matches!(
        engine
            .drop_index(&request("drop"), &rebuilding.handle)
            .await,
        Err(Error::Uncertain { .. })
    ));
    let dropped = engine
        .resolve_index_operation(&request("drop"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dropped.state, IndexState::Dropped);
    assert_eq!(
        engine
            .create_index(&request("create"), definition.clone())
            .await
            .unwrap(),
        created
    );
    assert_eq!(
        engine
            .rebuild_index(&request("rebuild"), &active.handle)
            .await
            .unwrap(),
        rebuilding
    );
    assert_eq!(
        engine
            .drop_index(&request("drop"), &rebuilding.handle)
            .await
            .unwrap(),
        dropped
    );
    faults.lose.store(1, Ordering::SeqCst);
    engine
        .create_index(&request("resolved-inside-call"), definition.clone())
        .await
        .unwrap();
    engine
        .drop_index(
            &request("remove"),
            &engine
                .get_index(definition.id())
                .await
                .unwrap()
                .unwrap()
                .candidate
                .unwrap()
                .handle,
        )
        .await
        .unwrap();
    let single = Engine::open(
        faults.clone(),
        EngineOptions {
            max_attempts: 1,
            ..EngineOptions::default()
        },
    )
    .await
    .unwrap();
    faults.lose.store(2, Ordering::SeqCst);
    let original = single
        .create_index(&request("concealed"), definition.clone())
        .await
        .unwrap();
    assert_eq!(
        single
            .create_index(&request("concealed"), definition.clone())
            .await
            .unwrap(),
        original
    );
    single
        .drop_index(&request("remove-concealed"), &original.handle)
        .await
        .unwrap();
    faults.fail_create.store(true, Ordering::SeqCst);
    let before = engine.snapshot().await.unwrap().reference();
    assert!(matches!(
        engine
            .create_index(&request("prepublication-failure"), definition)
            .await,
        Err(Error::Storage(obelisk_storage::Error::Transport(_)))
    ));
    assert_eq!(engine.snapshot().await.unwrap().reference(), before);
    assert!(
        engine
            .resolve_index_operation(&request("prepublication-failure"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_nonfinal_chunk_conflict_reconciles_processed_unscanned_deleted_and_new_paths() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder(&dir).await;
    let engine = open(store.clone()).await;
    engine
        .commit(&tx(
            "seed-nonfinal",
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
        .create_index(&request("create-nonfinal"), definition.clone())
        .await
        .unwrap();
    let gate = Arc::new(FaultStore::new(store));
    let builder = open(gate.clone()).await;
    gate.gate.store(true, Ordering::SeqCst);
    let handle = created.handle.clone();
    let task = tokio::spawn(async move {
        builder
            .advance_index_build(
                &request("nonfinal-step"),
                &handle,
                IndexBuildOptions::new(1).unwrap(),
            )
            .await
    });
    gate.wait().await;
    engine
        .commit(&tx(
            "during-nonfinal",
            vec![
                put("people/a", Value::Integer(10)),
                put("people/b", Value::Integer(20)),
                Mutation {
                    path: path("people/c"),
                    condition: Condition::Any,
                    kind: MutationKind::Delete,
                },
                put("people/d", Value::Integer(40)),
            ],
        ))
        .await
        .unwrap();
    gate.release.notify_one();
    let step = task.await.unwrap().unwrap();
    assert_eq!(gate.conflicts.load(Ordering::SeqCst), 1);
    assert_eq!(step.state, IndexState::Building);
    assert_eq!(step.progress.examined(), 1);
    assert_eq!(step.progress.last_path(), Some(&path("people/a")));
    let candidate = engine
        .get_index(definition.id())
        .await
        .unwrap()
        .unwrap()
        .candidate
        .unwrap();
    assert_eq!(candidate.progress, step.progress);
    let active = finish(&engine, &created.handle, "nonfinal-rest", 1).await;
    assert_eq!(active.progress.examined(), 3);
    assert_rows(
        &engine.snapshot().await.unwrap(),
        definition.id(),
        &[("people/a", 10), ("people/b", 20), ("people/d", 40)],
    )
    .await;
    assert_eq!(
        engine
            .advance_index_build(
                &request("nonfinal-step"),
                &created.handle,
                IndexBuildOptions::new(1).unwrap(),
            )
            .await
            .unwrap(),
        step,
        "replaying the nonfinal step must retain its original receipt after activation"
    );
}
