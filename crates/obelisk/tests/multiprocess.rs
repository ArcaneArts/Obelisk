//! Same executable is invoked as an isolated worker by parent tests. Workers
//! intentionally stop at markers so the parent can terminate them without cleanup.
#![cfg(any(feature = "folder", feature = "s3"))]
use obelisk::{
    storage::{self, ObjectId, Storage, VersionToken},
    *,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
enum Backend {
    #[cfg(feature = "folder")]
    Folder(PathBuf),
    #[cfg(feature = "s3")]
    S3 { bucket: String, prefix: String },
}
impl Backend {
    #[cfg(all(unix, feature = "folder"))]
    fn folder_path(&self) -> Option<&Path> {
        match self {
            Self::Folder(path) => Some(path),
            #[cfg(feature = "s3")]
            Self::S3 { .. } => None,
        }
    }
    fn isolated(&self, name: &str) -> Self {
        match self {
            #[cfg(feature = "folder")]
            Self::Folder(path) => Self::Folder(path.join(name)),
            #[cfg(feature = "s3")]
            Self::S3 { bucket, prefix } => Self::S3 {
                bucket: bucket.clone(),
                prefix: format!("{prefix}/{name}-{}", uuid::Uuid::new_v4()),
            },
        }
    }
    async fn store(&self) -> Arc<dyn Storage> {
        match self {
            #[cfg(feature = "folder")]
            Self::Folder(path) => Arc::new(storage::folder::Folder::open(path).await.unwrap()),
            #[cfg(feature = "s3")]
            Self::S3 { bucket, prefix } => {
                Arc::new(storage::s3::S3::from_env(bucket, prefix).unwrap())
            }
        }
    }
    async fn open(&self) -> Database {
        Database::open_with_storage(self.store().await, options())
            .await
            .unwrap()
    }
    fn configure(&self, command: &mut Command) {
        match self {
            #[cfg(feature = "folder")]
            Self::Folder(path) => {
                command
                    .env("OBELISK_QA_BACKEND", "folder")
                    .env("OBELISK_QA_FOLDER", path);
            }
            #[cfg(feature = "s3")]
            Self::S3 { bucket, prefix } => {
                command
                    .env("OBELISK_QA_BACKEND", "s3")
                    .env("OBELISK_S3_BUCKET", bucket)
                    .env("OBELISK_QA_PREFIX", prefix);
            }
        }
    }
    fn from_env() -> Self {
        match std::env::var("OBELISK_QA_BACKEND").unwrap().as_str() {
            #[cfg(feature = "folder")]
            "folder" => Self::Folder(std::env::var_os("OBELISK_QA_FOLDER").unwrap().into()),
            #[cfg(feature = "s3")]
            "s3" => Self::S3 {
                bucket: std::env::var("OBELISK_S3_BUCKET").unwrap(),
                prefix: std::env::var("OBELISK_QA_PREFIX").unwrap(),
            },
            _ => panic!("unknown worker backend"),
        }
    }
}
fn options() -> DatabaseOptions {
    DatabaseOptions {
        max_attempts: 128,
        ..Default::default()
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn path(p: &str) -> DocumentPath {
    DocumentPath::parse(p).unwrap()
}
fn document(value: i64) -> Document {
    Document::new([(FieldPath::parse("value").unwrap(), Value::Integer(value))]).unwrap()
}
fn transaction(id: &str, paths: &[&str], value: i64) -> Transaction {
    Transaction::new(
        RequestId::new(id).unwrap(),
        paths
            .iter()
            .map(|p| Mutation {
                path: path(p),
                condition: Condition::Any,
                kind: MutationKind::Put(document(value)),
            })
            .collect(),
        vec![],
    )
    .unwrap()
}
async fn number(snapshot: &Snapshot, p: &str) -> Option<i64> {
    snapshot.get(&path(p)).await.unwrap().map(|d| {
        match d.document.get(&FieldPath::parse("value").unwrap()).unwrap() {
            Value::Integer(v) => *v,
            _ => panic!("wrong value type"),
        }
    })
}
fn gate(ready: &Path, release: &Path, label: &str) {
    let temporary = ready.with_extension("tmp");
    std::fs::write(&temporary, label).unwrap();
    std::fs::rename(temporary, ready).unwrap();
    let start = Instant::now();
    while !release.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "worker gate timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[derive(Debug)]
struct FaultStore {
    inner: Arc<dyn Storage>,
    armed: AtomicBool,
    creates: AtomicUsize,
    stage: String,
    ready: PathBuf,
    release: PathBuf,
}
impl FaultStore {
    fn point(&self, stage: &str) {
        if self.armed.load(Ordering::SeqCst) && self.stage == stage {
            gate(&self.ready, &self.release, stage);
        }
    }
}
#[async_trait::async_trait]
impl Storage for FaultStore {
    async fn read(
        &self,
        id: ObjectId,
        range: Option<std::ops::Range<u64>>,
    ) -> storage::Result<bytes::Bytes> {
        let metadata = range.is_none();
        let bytes = self.inner.read(id, range).await?;
        if metadata {
            self.point("after-snapshot-read");
        }
        Ok(bytes)
    }
    async fn create(&self, id: ObjectId, bytes: bytes::Bytes) -> storage::Result<()> {
        if !self.armed.load(Ordering::SeqCst) {
            return self.inner.create(id, bytes).await;
        }
        let first = self.creates.fetch_add(1, Ordering::SeqCst) == 0;
        self.point(if first {
            "before-segment"
        } else {
            "before-manifest"
        });
        self.inner.create(id, bytes).await?;
        self.point(if first {
            "after-segment"
        } else {
            "after-manifest"
        });
        Ok(())
    }
    async fn read_head(&self) -> storage::Result<Option<storage::Head>> {
        let head = self.inner.read_head().await?;
        self.point("after-head-read");
        Ok(head)
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: bytes::Bytes,
    ) -> storage::Result<VersionToken> {
        self.point("before-publication");
        let version = self.inner.compare_exchange_head(expected, bytes).await?;
        self.point("after-publication");
        Ok(version)
    }
    async fn list(&self) -> storage::Result<Vec<ObjectId>> {
        self.inner.list().await
    }
    async fn delete(&self, id: ObjectId) -> storage::Result<()> {
        self.inner.delete(id).await
    }
}

#[test]
fn worker_process() {
    let Ok(mode) = std::env::var("OBELISK_QA_MODE") else {
        return;
    };
    let ready = PathBuf::from(std::env::var_os("OBELISK_QA_READY").unwrap());
    let release = PathBuf::from(std::env::var_os("OBELISK_QA_RELEASE").unwrap());
    if std::env::var_os("OBELISK_QA_START_GATE").is_some() {
        gate(&ready, &release, "ready");
    }
    let backend = Backend::from_env();
    let writer: usize = std::env::var("OBELISK_QA_WRITER")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap();
    runtime().block_on(async {
        let stage = std::env::var("OBELISK_QA_STAGE").unwrap_or_default();
        let fault = Arc::new(FaultStore {
            inner: backend.store().await,
            armed: AtomicBool::new(false),
            creates: AtomicUsize::new(0),
            stage: stage.clone(),
            ready: ready.clone(),
            release: release.clone(),
        });
        let db = Database::open_with_storage(fault.clone(), options())
            .await
            .unwrap();
        if mode == "crash" {
            #[cfg(all(unix, feature = "folder"))]
            if stage == "holding-folder-lock" {
                let root = backend
                    .folder_path()
                    .expect("folder lock mode requires folder");
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(root.join("LOCK"))
                    .unwrap();
                file.lock().unwrap();
                gate(&ready, &release, "holding-folder-lock");
                drop(file);
                return;
            }
            fault.armed.store(true, Ordering::SeqCst);
            let receipt = db
                .commit(&transaction("crash", &["crash/a", "crash/b"], 7))
                .await
                .unwrap();
            if stage == "after-ack" {
                gate(&ready, &release, &format!("ack:{}", receipt.commit_id));
            }
            panic!("fault stage was not reached: {stage}");
        } else if mode == "distinct" {
            for i in 0..4 {
                db.commit(&transaction(
                    &format!("writer-{writer}-{i}"),
                    &[
                        &format!("pairs/{writer}/{i}/a"),
                        &format!("pairs/{writer}/{i}/b"),
                    ],
                    (writer * 10 + i) as i64,
                ))
                .await
                .unwrap();
            }
        } else if mode == "duplicate" {
            db.create("same-request", "shared", document(123))
                .await
                .unwrap();
        } else if mode == "counter" {
            for i in 0..4 {
                let id = format!("increment-{writer}-{i}");
                let mut success = false;
                for _ in 0..200 {
                    let previous = db.get("counter").await.unwrap().unwrap();
                    let value = match previous
                        .document
                        .get(&FieldPath::parse("value").unwrap())
                        .unwrap()
                    {
                        Value::Integer(v) => *v,
                        _ => panic!("counter type"),
                    };
                    match db
                        .put(
                            &id,
                            "counter",
                            document(value + 1),
                            Condition::Revision(previous.revision),
                        )
                        .await
                    {
                        Ok(_) => {
                            success = true;
                            break;
                        }
                        Err(Error::RevisionConflict(_)) => (),
                        other => panic!("counter failed: {other:?}"),
                    }
                }
                assert!(success, "counter retry budget exhausted");
            }
        } else {
            panic!("unknown worker mode");
        }
    });
}

struct Worker {
    child: Option<Child>,
    ready: PathBuf,
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Worker {
    fn spawn(
        backend: &Backend,
        signals: &Path,
        mode: &str,
        writer: usize,
        stage: &str,
        start_gate: bool,
        release: &Path,
    ) -> Self {
        let ready = signals.join(format!("{mode}-{writer}-{stage}.ready"));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "worker_process", "--nocapture"])
            .env("OBELISK_QA_MODE", mode)
            .env("OBELISK_QA_READY", &ready)
            .env("OBELISK_QA_RELEASE", release)
            .env("OBELISK_QA_WRITER", writer.to_string())
            .env("OBELISK_QA_STAGE", stage)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if start_gate {
            command.env("OBELISK_QA_START_GATE", "1");
        } else {
            command.env_remove("OBELISK_QA_START_GATE");
        }
        backend.configure(&mut command);
        Self {
            child: Some(command.spawn().unwrap()),
            ready,
        }
    }
    fn marker(&mut self) -> String {
        let start = Instant::now();
        while !self.ready.exists() {
            if self.exited() {
                self.finish();
                panic!("worker exited without marker");
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "worker marker timed out: {}",
                self.ready.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::read_to_string(&self.ready).unwrap()
    }
    fn exited(&mut self) -> bool {
        self.child.as_mut().unwrap().try_wait().unwrap().is_some()
    }
    fn finish(&mut self) {
        let start = Instant::now();
        while !self.exited() {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "worker timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "worker failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn kill(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
    }
}
fn launch_group(backend: &Backend, signals: &Path, mode: &str, count: usize) -> Vec<Worker> {
    let release = signals.join(format!("{mode}.release"));
    let mut workers: Vec<_> = (0..count)
        .map(|i| Worker::spawn(backend, signals, mode, i, "", true, &release))
        .collect();
    for worker in &mut workers {
        assert_eq!(worker.marker(), "ready");
    }
    std::fs::write(release, "go").unwrap();
    workers
}
async fn suite(backend: Backend, signals: &Path) {
    let distinct = backend.isolated("distinct");
    let mut workers = launch_group(&distinct, signals, "distinct", 6);
    let db = distinct.open().await;
    let mut reads = 0;
    let start = Instant::now();
    loop {
        let snapshot = db.snapshot().await.unwrap();
        for writer in 0..6 {
            for i in 0..4 {
                let a = number(&snapshot, &format!("pairs/{writer}/{i}/a")).await;
                let b = number(&snapshot, &format!("pairs/{writer}/{i}/b")).await;
                assert_eq!(a, b, "torn batch in pinned snapshot");
            }
        }
        reads += 1;
        if workers.iter_mut().all(Worker::exited) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "concurrent writers timed out"
        );
    }
    for worker in &mut workers {
        worker.finish();
    }
    let snapshot = distinct.open().await.snapshot().await.unwrap();
    assert_eq!(snapshot.sequence(), 24);
    for writer in 0..6 {
        for i in 0..4 {
            assert_eq!(
                number(&snapshot, &format!("pairs/{writer}/{i}/a")).await,
                Some((writer * 10 + i) as i64)
            );
        }
    }
    eprintln!(
        "multiprocess: 6 writers, 24 acknowledged atomic pairs, {reads} pinned consistency scans"
    );

    let duplicate = backend.isolated("duplicate");
    let mut workers = launch_group(&duplicate, signals, "duplicate", 6);
    for worker in &mut workers {
        worker.finish();
    }
    let db = duplicate.open().await;
    assert_eq!(db.snapshot().await.unwrap().sequence(), 1);
    assert_eq!(
        db.resolve("same-request").await.unwrap().unwrap().sequence,
        1
    );
    eprintln!("multiprocess: 6 identical requests committed once");

    let counter = backend.isolated("counter");
    let db = counter.open().await;
    db.create("seed-counter", "counter", document(0))
        .await
        .unwrap();
    let mut workers = launch_group(&counter, signals, "counter", 4);
    for worker in &mut workers {
        worker.finish();
    }
    let snapshot = counter.open().await.snapshot().await.unwrap();
    assert_eq!(number(&snapshot, "counter").await, Some(16));
    assert_eq!(snapshot.sequence(), 17);
    eprintln!("multiprocess: 4 contending writers preserved 16 conditional increments");

    for stage in [
        "after-head-read",
        "after-snapshot-read",
        "before-segment",
        "after-segment",
        "before-manifest",
        "after-manifest",
        "before-publication",
        "after-publication",
        "after-ack",
    ] {
        let target = backend.isolated(stage);
        let db = target.open().await;
        let seed = db.create("seed", "seed", document(99)).await.unwrap();
        let release = signals.join(format!("{stage}.release"));
        let mut worker = Worker::spawn(&target, signals, "crash", 0, stage, false, &release);
        let marker = worker.marker();
        worker.kill();
        let reopened = target.open().await;
        let snapshot = reopened.snapshot().await.unwrap();
        assert_eq!(number(&snapshot, "seed").await, Some(99));
        assert_eq!(reopened.resolve("seed").await.unwrap(), Some(seed));
        let a = number(&snapshot, "crash/a").await;
        let b = number(&snapshot, "crash/b").await;
        assert_eq!(a, b);
        let published = stage == "after-publication" || stage == "after-ack";
        assert_eq!(a.is_some(), published, "unexpected publication at {stage}");
        let outcome = reopened.resolve("crash").await.unwrap();
        assert_eq!(outcome.is_some(), published);
        if stage == "after-ack" {
            assert_eq!(
                marker,
                format!("ack:{}", outcome.as_ref().unwrap().commit_id)
            );
        }
        let receipt = reopened
            .commit(&transaction("crash", &["crash/a", "crash/b"], 7))
            .await
            .unwrap();
        if let Some(original) = outcome {
            assert_eq!(receipt, original);
        }
        assert_eq!(receipt.sequence, 2);
        assert_eq!(reopened.snapshot().await.unwrap().sequence(), 2);
        eprintln!("process kill: {stage}; preserved seed, atomic visibility, retry receipt");
    }
    #[cfg(all(unix, feature = "folder"))]
    if matches!(backend, Backend::Folder(_)) {
        let target = backend.isolated("lock-death");
        let db = target.open().await;
        db.create("seed", "seed", document(1)).await.unwrap();
        let release = signals.join("lock.release");
        let mut worker = Worker::spawn(
            &target,
            signals,
            "crash",
            0,
            "holding-folder-lock",
            false,
            &release,
        );
        assert_eq!(worker.marker(), "holding-folder-lock");
        worker.kill();
        target
            .open()
            .await
            .put("after-lock-death", "alive", document(2), Condition::Any)
            .await
            .unwrap();
        eprintln!("process kill: OS publication lock released on writer death");
    }
}
#[cfg(all(feature = "folder", unix))]
#[test]
fn folder_multiprocess_and_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let signals = tempfile::tempdir().unwrap();
    runtime().block_on(suite(
        Backend::Folder(dir.path().to_owned()),
        signals.path(),
    ));
}
#[cfg(feature = "s3")]
#[test]
#[ignore = "requires actual AWS S3 bucket, isolated prefix, and external credentials"]
fn real_s3_multiprocess_and_crash_recovery() {
    assert!(
        std::env::var_os("AWS_ENDPOINT").is_none()
            && std::env::var_os("AWS_ENDPOINT_URL").is_none(),
        "custom endpoints require separately labeled emulator tests"
    );
    let bucket = std::env::var("OBELISK_S3_BUCKET").expect("set test bucket");
    let base = std::env::var("OBELISK_S3_TEST_PREFIX").expect("set isolated test prefix");
    let prefix = format!("{base}/process-{}", uuid::Uuid::new_v4());
    eprintln!("real S3 multiprocess prefix retained: {prefix}");
    let signals = tempfile::tempdir().unwrap();
    runtime().block_on(suite(Backend::S3 { bucket, prefix }, signals.path()));
}
