use super::*;
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[tokio::test]
async fn relative_root_is_pinned_when_opened() {
    let directory = tempfile::tempdir_in(".").unwrap();
    let store = Folder::open(directory.path()).await.unwrap();
    assert!(store.root.is_absolute());
    assert_eq!(store.root, directory.path().canonicalize().unwrap());
    let id = ObjectId::random();
    store.create(id, "stable-root".into()).await.unwrap();
    assert_eq!(store.read(id, None).await.unwrap(), "stable-root");
}

#[tokio::test]
async fn interrupted_publication_requires_a_successful_read_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let store = Folder::open(directory.path()).await.unwrap();
    let original = store
        .compare_exchange_head(None, "original".into())
        .await
        .unwrap();
    let fail_sync = |_: &Path| Err(std::io::Error::other("injected directory sync failure"));
    assert!(matches!(
        publish_head(&store.root, Some(&original), b"renamed", fail_sync),
        Err(Error::Uncertain(_))
    ));
    // Visibility alone must not turn an uncertain publication into an
    // acknowledged receipt if the recovering reader cannot complete the sync.
    assert_eq!(head(&store.root).unwrap().unwrap().bytes, "renamed");
    assert!(matches!(
        durable_head(&store.root, fail_sync),
        Err(Error::Io(_))
    ));
    let mut barrier_ran = false;
    let observed = durable_head(&store.root, |root| {
        barrier_ran = true;
        sync_dir(root)
    })
    .unwrap()
    .unwrap();
    assert!(barrier_ran);
    assert_eq!(observed.bytes, "renamed");
    assert_ne!(observed.version, original);
    assert!(matches!(
        store
            .compare_exchange_head(Some(&original), "stale".into())
            .await,
        Err(Error::Conflict)
    ));
    store
        .compare_exchange_head(Some(&observed.version), "next".into())
        .await
        .unwrap();
}

// Only the subprocess receives these variables; no global fault switches or
// environment changes are installed in the parent test process.
#[test]
fn publication_crash_worker() {
    let Some(root) = std::env::var_os("OBELISK_FOLDER_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let marker = std::env::var_os("OBELISK_FOLDER_CRASH_MARKER").unwrap();
    let expected = durable_head(&root, sync_dir).unwrap().unwrap().version;
    publish_head(&root, Some(&expected), b"interrupted", |_| {
        // The same production publication helper has renamed HEAD, but has
        // not synced the directory or released its publication lock yet.
        fs::write(marker, "after-head-rename").unwrap();
        loop {
            std::thread::park();
        }
    })
    .unwrap();
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn killed_publisher_is_recovered_by_an_already_open_reader() {
    let directory = tempfile::tempdir().unwrap();
    let signals = tempfile::tempdir().unwrap();
    let marker = signals.path().join("renamed");
    // Open before the interrupted rename: adapter setup cannot mask a missing
    // barrier by syncing the directory again during a fresh Folder::open.
    let reader = Folder::open(directory.path()).await.unwrap();
    let original = reader
        .compare_exchange_head(None, "acknowledged".into())
        .await
        .unwrap();
    let mut worker = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "folder::tests::publication_crash_worker"])
            .env("OBELISK_FOLDER_CRASH_ROOT", &reader.root)
            .env("OBELISK_FOLDER_CRASH_MARKER", &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "worker exited early"
        );
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "worker timed out"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    worker.0.kill().unwrap();
    assert!(!worker.0.wait().unwrap().success());
    let recovered = reader.read_head().await.unwrap().unwrap();
    assert_eq!(recovered.bytes, "interrupted");
    assert_ne!(recovered.version, original);
    reader
        .compare_exchange_head(Some(&recovered.version), "after-recovery".into())
        .await
        .unwrap();
    assert_eq!(
        reader.read_head().await.unwrap().unwrap().bytes,
        "after-recovery"
    );
}
